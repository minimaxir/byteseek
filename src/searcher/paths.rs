use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::Utf8Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use smallvec::SmallVec;

use super::{Engine, Searcher};
use crate::chunks::map_nested_chunks;
use crate::profiles;
use crate::{
    FileMatchResult, OwnedStructuredMatch, SearchError, StructuredFileMatchResult, StructuredMatch,
};

pub(crate) struct ScannedTextFile<T> {
    pub(crate) path: PathBuf,
    pub(crate) byte_len: usize,
    pub(crate) value: Option<T>,
}

/// Whether `path` is ignored. The innermost `.gitignore` with a matching rule decides.
fn is_gitignored(path: &Path, is_dir: bool, gitignores: &[Arc<Gitignore>]) -> bool {
    gitignores
        .iter()
        .rev()
        .map(|gitignore| gitignore.matched(path, is_dir))
        .find(|matched| !matched.is_none())
        .is_some_and(|matched| matched.is_ignore())
}

/// Length of the first read. Larger files are checked for invalid UTF-8 within it before the
/// rest is read.
const FIRST_READ_LEN: usize = 16 * 1024;

/// Why a file could not be read as UTF-8 text.
#[derive(Debug, thiserror::Error)]
enum TextReadError {
    /// The file could not be opened or read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The file's contents are not UTF-8.
    #[error(transparent)]
    NotUtf8(#[from] Utf8Error),
}

/// Reads a file as UTF-8 text.
fn read_text_file(path: &Path) -> Result<String, TextReadError> {
    let file = fs::File::open(path)?;
    read_text(&file)
}

/// Reads all of `reader` as UTF-8 text.
///
/// The first read goes through `Take`, whose missing size hint skips the `fstat` and `lseek`
/// that `File::read_to_end` makes, so small files cost only their reads. Inputs with invalid
/// UTF-8 in the first read fail without being read in full.
fn read_text(mut reader: impl Read) -> Result<String, TextReadError> {
    let mut bytes = Vec::with_capacity(FIRST_READ_LEN);
    reader
        .by_ref()
        .take(FIRST_READ_LEN as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() == FIRST_READ_LEN {
        // An invalid sequence here fails the full check with the same error. `error_len` is
        // `None` only for a character cut off by the end of the read.
        if let Err(error) = std::str::from_utf8(&bytes)
            && error.error_len().is_some()
        {
            return Err(error.into());
        }
        reader.read_to_end(&mut bytes)?;
    }
    String::from_utf8(bytes).map_err(|error| error.utf8_error().into())
}

/// Bounds how many files and subdirectories of one traversal are visited concurrently.
struct WalkBudget {
    /// Walks running beside the one that started the traversal.
    extra_walks: AtomicUsize,
    limit: usize,
}

impl WalkBudget {
    fn new(limit: usize) -> Self {
        Self {
            extra_walks: AtomicUsize::new(0),
            limit,
        }
    }

    /// Reserves up to `wanted` extra walks and returns how many were granted. The caller
    /// returns them with [`Self::release`].
    fn reserve(&self, wanted: usize) -> usize {
        let mut granted = 0;
        // `try_update` is a compare-and-swap loop; returning `None` leaves the count as is.
        let _ = self
            .extra_walks
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |running| {
                granted = wanted.min(self.limit.saturating_sub(running + 1));
                (granted > 0).then_some(running + granted)
            });
        granted
    }

    fn release(&self, granted: usize) {
        self.extra_walks.fetch_sub(granted, Ordering::Relaxed);
    }
}

/// State shared by every directory of one traversal.
struct Walk<'a, F> {
    searcher: &'a Searcher,
    ignore_gitignore: bool,
    budget: WalkBudget,
    search_fn: &'a F,
}

impl<T, F> Walk<'_, F>
where
    T: Send,
    F: Fn(&Searcher, &Path, &str) -> Result<T, SearchError> + Sync,
{
    /// Recursively walks `start_directory`, appending one result per file to `results`.
    ///
    /// Results are in discovery order: a directory's own files, then its subdirectories. A
    /// lone subdirectory is descended in a loop instead of recursing.
    fn directory(
        &self,
        start_directory: &Path,
        starting_gitignores: &[Arc<Gitignore>],
        results: &mut Vec<T>,
    ) -> Result<(), SearchError> {
        let mut current_dir = start_directory.to_path_buf();
        let mut gitignore_stack: Vec<Arc<Gitignore>> = starting_gitignores.to_vec();
        loop {
            // Spotting `.gitignore` in the listing avoids an `open` per directory.
            let mut raw_entries = Vec::new();
            let mut has_gitignore = false;
            for entry in fs::read_dir(&current_dir)
                .map_err(|error| file_traversal_error(&current_dir, error))?
            {
                let entry = entry.map_err(|error| file_traversal_error(&current_dir, error))?;
                if entry.file_name() == ".gitignore" {
                    has_gitignore = true;
                    continue;
                }
                raw_entries.push(entry);
            }
            if !self.ignore_gitignore && has_gitignore {
                let mut builder = GitignoreBuilder::new(&current_dir);
                if let Some(error) = builder.add(current_dir.join(".gitignore")) {
                    return Err(SearchError::FileTraversal {
                        path: current_dir.join(".gitignore").display().to_string(),
                        message: error.to_string(),
                    });
                }
                let matcher = builder
                    .build()
                    .map_err(|error| file_traversal_error(&current_dir, error))?;
                if !matcher.is_empty() {
                    gitignore_stack.push(Arc::new(matcher));
                }
            }
            // Classify entries only once this directory's rules are loaded.
            let mut files: SmallVec<[PathBuf; 8]> = SmallVec::new();
            let mut subdirs: SmallVec<[PathBuf; 4]> = SmallVec::new();
            for entry in raw_entries {
                let file_type = entry
                    .file_type()
                    .map_err(|error| file_traversal_error(&current_dir, error))?;
                let is_dir = file_type.is_dir();
                let path = entry.path();
                if !self.ignore_gitignore
                    && is_gitignored(path.as_path(), is_dir, gitignore_stack.as_slice())
                {
                    continue;
                }
                if is_dir {
                    subdirs.push(path);
                } else if file_type.is_file() {
                    files.push(path);
                }
            }
            self.in_groups(&files, results, |path, results| self.file(path, results))?;
            match subdirs.as_mut_slice() {
                [] => return Ok(()),
                [only] => current_dir = std::mem::take(only),
                dirs => {
                    return self.in_groups(dirs, results, |dir, results| {
                        self.directory(dir, &gitignore_stack, results)
                    });
                }
            }
        }
    }

    /// Reads and searches one file, skipping it if it is not UTF-8.
    fn file(&self, path: &Path, results: &mut Vec<T>) -> Result<(), SearchError> {
        let text = match read_text_file(path) {
            Ok(text) => text,
            Err(TextReadError::NotUtf8(_)) => return Ok(()),
            Err(TextReadError::Io(error)) => return Err(file_read_error(path, error)),
        };
        results.push((self.search_fn)(self.searcher, path, &text)?);
        Ok(())
    }

    /// Visits `paths` in order, split into as many parallel groups as the budget grants.
    fn in_groups(
        &self,
        paths: &[PathBuf],
        results: &mut Vec<T>,
        visit: impl Fn(&Path, &mut Vec<T>) -> Result<(), SearchError> + Sync,
    ) -> Result<(), SearchError> {
        let visit_group = |group: &[PathBuf], results: &mut Vec<T>| {
            group.iter().try_for_each(|path| visit(path, results))
        };
        let granted = match paths.len() {
            0 | 1 => 0,
            len => self.budget.reserve(len - 1),
        };
        if granted == 0 {
            return visit_group(paths, results);
        }
        let group_len = paths.len().div_ceil(granted + 1);
        // Rounding the group length up can leave fewer groups than walks reserved.
        let group_count = paths.len().div_ceil(group_len);
        self.budget.release(granted + 1 - group_count);
        // Nested, since each group may fan out again.
        let groups = map_nested_chunks(group_count, |group_index| {
            let group = &paths[group_index * group_len..];
            let mut visited = Vec::new();
            let outcome = visit_group(&group[..group.len().min(group_len)], &mut visited);
            // The first group runs on the caller's walk. Others return theirs as soon as they
            // end, so a group still walking can fan out again.
            if group_index > 0 {
                self.budget.release(1);
            }
            outcome.map(|()| visited)
        });
        for group in groups {
            results.extend(group?);
        }
        Ok(())
    }
}

impl Searcher {
    /// Searches a file or recursively searches a directory for matching files.
    ///
    /// Traversal respects `.gitignore` files unless
    /// [`crate::SearchConfigBuilder::ignore_gitignore`] is set. Files found in a directory are
    /// skipped unless they are valid UTF-8; [`Searcher::count_matching_lines_in_reader`] can
    /// count lines in such files.
    ///
    /// # Arguments
    ///
    /// * `path` - File or directory to search.
    ///
    /// # Returns
    ///
    /// One result per file with at least one match.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::FileTraversal`] when the path cannot be walked.
    /// Returns [`SearchError::FileRead`] when a file cannot be read, or when `path` itself
    /// names a file that is not UTF-8 text.
    /// Returns [`SearchError::StructuredParse`] when structured mode is enabled and a candidate
    /// file fails to parse.
    pub fn find_path<P: AsRef<Path>>(&self, path: P) -> Result<Vec<FileMatchResult>, SearchError> {
        let mut results =
            self.search_text_files(path.as_ref(), |searcher, file_path, contents| {
                let mut matches = searcher.try_find(contents)?;
                if matches.is_empty() {
                    Ok(None)
                } else {
                    // Results outlive the walk, so drop spare capacity.
                    matches.shrink_to_fit();
                    Ok(Some(FileMatchResult {
                        path: file_path.to_path_buf(),
                        matches,
                    }))
                }
            })?;
        if self.config.path_order() == crate::PathOrder::Path {
            results.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        }
        Ok(results)
    }

    /// Searches a file or recursively searches a directory for structured matches.
    ///
    /// Traversal respects `.gitignore` files unless
    /// [`crate::SearchConfigBuilder::ignore_gitignore`] is set. Files found in a directory are
    /// skipped unless they are valid UTF-8.
    ///
    /// # Arguments
    ///
    /// * `path` - File or directory to search.
    ///
    /// # Returns
    ///
    /// One result per file with at least one structured match.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredModeDisabled`] when the searcher was built without a
    /// structured mode.
    /// Returns [`SearchError::FileTraversal`] when the path cannot be walked.
    /// Returns [`SearchError::FileRead`] when a file cannot be read, or when `path` itself
    /// names a file that is not UTF-8 text.
    /// Returns [`SearchError::StructuredParse`] when a candidate file fails to parse.
    pub fn find_structured_path<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Vec<StructuredFileMatchResult>, SearchError> {
        let results = self.find_structured_path_with(path.as_ref(), |matches, _| {
            matches
                .into_iter()
                .map(OwnedStructuredMatch::from)
                .collect()
        })?;
        Ok(results
            .into_iter()
            .map(|(path, matches)| StructuredFileMatchResult { path, matches })
            .collect())
    }

    /// Like [`Self::find_structured_path`], turning each file's matches into `T`s with
    /// `convert`, which also sees the file's text.
    pub(crate) fn find_structured_path_with<T, F>(
        &self,
        path: &Path,
        convert: F,
    ) -> Result<Vec<(PathBuf, Vec<T>)>, SearchError>
    where
        T: Send,
        F: Fn(Vec<StructuredMatch<'_>>, &str) -> Vec<T> + Sync + Send,
    {
        if !matches!(self.engine, Engine::Structured(_)) {
            return Err(SearchError::StructuredModeDisabled);
        }

        let mut results =
            self.search_text_files(path, |searcher, file_path, contents| {
                let matches = searcher.find_structured(contents)?;
                Ok((!matches.is_empty())
                    .then(|| (file_path.to_path_buf(), convert(matches, contents))))
            })?;
        if self.config.path_order() == crate::PathOrder::Path {
            results.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        }
        Ok(results)
    }

    /// Like [`Self::scan_text_files`], keeping only the values `search` returned.
    fn search_text_files<T, F>(&self, root: &Path, search: F) -> Result<Vec<T>, SearchError>
    where
        T: Send,
        F: Fn(&Searcher, &Path, &str) -> Result<Option<T>, SearchError> + Sync + Send,
    {
        let scanned = self.scan_text_files(root, search)?;
        Ok(scanned
            .into_iter()
            .filter_map(|scanned_file| scanned_file.value)
            .collect())
    }

    /// Reads `root`, or every file under it, as UTF-8 text and records each file's size and
    /// search result, visiting files on several threads.
    ///
    /// Files under a directory that are not UTF-8 are skipped, but a non-UTF-8 `root` file is
    /// a [`SearchError::FileRead`].
    pub(crate) fn scan_text_files<T, F>(
        &self,
        root: &Path,
        search: F,
    ) -> Result<Vec<ScannedTextFile<T>>, SearchError>
    where
        T: Send,
        F: Fn(&Searcher, &Path, &str) -> Result<Option<T>, SearchError> + Sync + Send,
    {
        // One `stat` instead of separate `Path::is_file` and `Path::is_dir` calls.
        let root_type = fs::metadata(root)
            .map_err(|error| file_traversal_error(root, error))?
            .file_type();
        if root_type.is_file() {
            let text = read_text_file(root).map_err(|error| file_read_error(root, error))?;
            return Ok(vec![ScannedTextFile {
                path: root.to_path_buf(),
                byte_len: text.len(),
                value: search(self, root, &text)?,
            }]);
        }

        if !root_type.is_dir() {
            return Err(SearchError::FileTraversal {
                path: root.display().to_string(),
                message: "path is neither a file nor a directory".to_owned(),
            });
        }

        let mut scanned = Vec::new();
        Walk {
            searcher: self,
            ignore_gitignore: self.config.ignore_gitignore(),
            budget: WalkBudget::new(profiles::walk_parallelism()),
            search_fn: &|searcher: &Searcher, file_path: &Path, contents: &str| {
                Ok(ScannedTextFile {
                    path: file_path.to_path_buf(),
                    byte_len: contents.len(),
                    value: search(searcher, file_path, contents)?,
                })
            },
        }
        .directory(root, &[], &mut scanned)?;
        Ok(scanned)
    }
}

fn file_read_error(path: &Path, error: impl std::fmt::Display) -> SearchError {
    SearchError::FileRead {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn file_traversal_error(path: &Path, error: impl std::fmt::Display) -> SearchError {
    SearchError::FileTraversal {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_text_bytes(contents: &[u8]) -> Result<Vec<u8>, TextReadError> {
        read_text(std::io::Cursor::new(contents)).map(String::into_bytes)
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("unreadable"))
        }
    }

    #[test]
    fn reads_files_around_the_first_read_length_whole() {
        for len in [
            0,
            1,
            FIRST_READ_LEN - 1,
            FIRST_READ_LEN,
            FIRST_READ_LEN + 1,
            70_000,
        ] {
            let contents = vec![b'x'; len];

            assert_eq!(
                read_text_bytes(&contents).expect("text should read"),
                contents
            );
        }
    }

    #[test]
    fn character_cut_by_the_first_read_is_not_rejected_early() {
        let mut contents = vec![b'x'; FIRST_READ_LEN - 1];
        contents.extend_from_slice("\u{e9}tail".as_bytes());

        assert_eq!(
            read_text_bytes(&contents).expect("valid UTF-8 should read"),
            contents
        );
    }

    #[test]
    fn character_cut_by_the_first_read_is_rejected_if_its_rest_is_invalid() {
        // A two-byte character's lead byte ends the first read, then an ASCII byte follows.
        let mut contents = vec![b'x'; FIRST_READ_LEN - 1];
        contents.extend_from_slice(b"\xc3(tail");

        let error = read_text_bytes(&contents).expect_err("the character is never completed");

        assert!(matches!(error, TextReadError::NotUtf8(_)));
    }

    #[test]
    fn a_failed_read_is_told_from_contents_that_are_not_utf8() {
        let error = read_text(FailingReader).expect_err("the reader fails");

        assert!(matches!(error, TextReadError::Io(_)));
    }

    #[test]
    fn invalid_utf8_in_the_first_read_fails_like_the_full_check() {
        let mut contents = vec![0xff];
        contents.extend(std::iter::repeat_n(b'x', 3 * FIRST_READ_LEN));

        let error = read_text_bytes(&contents).expect_err("binary data should be rejected early");
        let expected = std::str::from_utf8(&contents).expect_err("contents are invalid UTF-8");

        assert!(matches!(error, TextReadError::NotUtf8(error) if error == expected));
    }

    #[test]
    fn walk_budget_never_grants_more_than_its_limit() {
        let budget = WalkBudget::new(4);

        assert_eq!(budget.reserve(2), 2);
        assert_eq!(budget.reserve(5), 1);
        assert_eq!(budget.reserve(1), 0);
        budget.release(2);
        assert_eq!(budget.reserve(5), 2);
        assert_eq!(WalkBudget::new(1).reserve(3), 0);
    }

    /// Contents whose search fails in [`walk_with_limit`].
    const UNSEARCHABLE: &str = "unsearchable";

    /// Walks `root` with at most `limit` concurrent visits, asserting the budget is returned.
    fn walk_with_limit(root: &Path, limit: usize) -> Result<Vec<(PathBuf, String)>, SearchError> {
        let searcher = Searcher::new(["needle"], crate::SearchConfig::default())
            .expect("test pattern should compile");
        let walk = Walk {
            searcher: &searcher,
            ignore_gitignore: false,
            budget: WalkBudget::new(limit),
            search_fn: &|_: &Searcher, path: &Path, contents: &str| {
                if contents == UNSEARCHABLE {
                    // An error no file read produces.
                    return Err(SearchError::StructuredParse {
                        mode: crate::StructuredMode::Json,
                        message: path.display().to_string(),
                    });
                }
                Ok((path.to_path_buf(), contents.to_owned()))
            },
        };
        let mut visited = Vec::new();
        let outcome = walk.directory(root, &[], &mut visited);
        assert_eq!(walk.budget.extra_walks.load(Ordering::Relaxed), 0);
        outcome.map(|()| visited)
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().expect("test files have a parent"))
            .expect("directory should build");
        fs::write(path, contents).expect("file should write");
    }

    #[test]
    fn every_walk_budget_visits_the_same_files_in_the_same_order() {
        let root = tempfile::TempDir::new().expect("temporary directory should build");
        // Small branches beside one deep, wide branch, with files beside subdirectories.
        for (branch, leaf) in (0..7).flat_map(|branch| (0..2).map(move |leaf| (branch, leaf))) {
            let file = format!("branch-{branch}/leaf-{leaf}/notes.txt");
            write_file(&root.path().join(file), b"needle\n");
        }
        for (level, file) in (0..6).flat_map(|level| (0..9).map(move |file| (level, file))) {
            let directory = "nested/".repeat(level + 1);
            let file = format!("large/{directory}side-{}/file-{file}.txt", level % 2);
            write_file(
                &root.path().join(file),
                format!("needle {level}\n").as_bytes(),
            );
        }

        let sequential = walk_with_limit(root.path(), 1).expect("walk should succeed");

        assert_eq!(sequential.len(), 14 + 54);
        for limit in [2, 3, 4, 6, 64] {
            let walked = walk_with_limit(root.path(), limit).expect("walk should succeed");
            assert_eq!(walked, sequential, "limit {limit}");
        }
    }

    /// Writes six one-file directories, the fourth holding `contents`, and returns its path.
    fn write_branches(root: &Path, contents: &[u8]) -> PathBuf {
        for branch in 0..6 {
            write_file(
                &root.join(format!("branch-{branch}/notes.txt")),
                if branch == 3 { contents } else { b"needle" },
            );
        }
        root.join("branch-3/notes.txt")
    }

    #[test]
    fn walks_fail_on_a_file_whose_search_fails_and_return_their_budget() {
        let root = tempfile::TempDir::new().expect("temporary directory should build");
        write_branches(root.path(), UNSEARCHABLE.as_bytes());

        for limit in [1, 4] {
            let error = walk_with_limit(root.path(), limit).expect_err("the search should fail");

            assert!(
                matches!(
                    &error,
                    SearchError::StructuredParse { message, .. } if message.contains("branch-3")
                ),
                "limit {limit}: {error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn walks_fail_on_a_file_that_cannot_be_opened_and_return_their_budget() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::TempDir::new().expect("temporary directory should build");
        let locked = write_branches(root.path(), b"needle");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("permissions should change");
        // The superuser opens every file, which leaves nothing to fail on.
        if fs::File::open(&locked).is_ok() {
            return;
        }

        for limit in [1, 4] {
            let error = walk_with_limit(root.path(), limit).expect_err("the open should fail");

            assert!(
                matches!(&error, SearchError::FileRead { path, .. } if path.contains("branch-3")),
                "limit {limit}: {error:?}"
            );
        }
    }

    #[test]
    fn walks_pass_over_files_that_are_not_utf8_and_return_their_budget() {
        let root = tempfile::TempDir::new().expect("temporary directory should build");
        let binary = write_branches(root.path(), b"\xff\xfe");

        for limit in [1, 4] {
            let walked = walk_with_limit(root.path(), limit).expect("walk should succeed");

            assert_eq!(walked.len(), 5, "limit {limit}");
            assert!(
                walked.iter().all(|(path, _)| *path != binary),
                "limit {limit}: {walked:?}"
            );
        }
    }

    #[test]
    fn innermost_gitignore_rule_decides() {
        let build = |root: &str, rule: &str| {
            let mut builder = GitignoreBuilder::new(root);
            builder.add_line(None, rule).expect("rule should parse");
            Arc::new(builder.build().expect("matcher should build"))
        };
        let outer_ignores = build("/repo", "*.log");
        let inner_keeps = build("/repo/app", "!*.log");
        let path = Path::new("/repo/app/run.log");

        assert!(!is_gitignored(
            path,
            false,
            &[outer_ignores.clone(), inner_keeps.clone()]
        ));
        assert!(is_gitignored(path, false, &[inner_keeps, outer_ignores]));
        assert!(!is_gitignored(
            Path::new("/repo/app/run.txt"),
            false,
            &[build("/repo", "*.log")]
        ));
    }
}
