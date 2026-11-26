//! When files last changed: their last commit when git tracks them, or else their
//! modification time. Results are ordered by it, and `--pretty` headings show it.
//!
//! Starting `git` costs far more than its history walk, so files are looked up in batches:
//! one `git log` per repository finds the last commit of every file in it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, fs};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rayon::prelude::*;

/// When a file last changed, and how that was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastChange {
    /// The time of the last commit that touched the file.
    Committed(Timestamp),
    /// The file's modification time, for a file outside git or never committed.
    Modified(Timestamp),
}

/// The local time zone, which headings show times in.
static LOCAL: LazyLock<TimeZone> = LazyLock::new(TimeZone::system);
/// Set once `git` fails to start, so later files go straight to their modification time.
static GIT_MISSING: AtomicBool = AtomicBool::new(false);
/// Starts each commit's record in `git log` output, before its committer time.
const COMMIT_MARK: u8 = 0x01;

impl LastChange {
    /// When the change happened.
    pub fn time(self) -> Timestamp {
        match self {
            Self::Committed(time) | Self::Modified(time) => time,
        }
    }

    /// The minute of the change since the Unix epoch, the precision headings show, so files
    /// that show the same time can be ordered another way.
    pub fn minute(self) -> i64 {
        self.time().as_second().div_euclid(60)
    }

    /// Describes the change in `zone` to the minute, such as `committed 2026-03-18 14:02`.
    pub fn describe(self, zone: &TimeZone) -> String {
        let verb = match self {
            Self::Committed(_) => "committed",
            Self::Modified(_) => "modified",
        };
        let local = zone.to_datetime(self.time());
        // Written from fields: a format string would link jiff's whole formatter.
        format!(
            "{verb} {:04}-{:02}-{:02} {:02}:{:02}",
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute()
        )
    }

    /// Describes the change in the local time zone.
    pub fn describe_local(self) -> String {
        self.describe(&LOCAL)
    }
}

/// A file to find in a repository's history: its index among the paths asked about, and its
/// path relative to the repository root as `git` prints it.
type TrackedFile = (usize, Vec<u8>);

/// Finds when each file in `paths` last changed.
///
/// # Returns
///
/// For each path in order: its last commit when it is in a git repository that has committed
/// it, else its modification time, or `None` when neither can be read.
pub fn last_changes(paths: &[&Path]) -> Vec<Option<LastChange>> {
    let mut changes = vec![None; paths.len()];
    if paths.is_empty() {
        return changes;
    }
    if !GIT_MISSING.load(Ordering::Relaxed) {
        for (root, files) in group_by_repository(paths) {
            commit_times(&root, &files, &mut changes);
        }
    }
    changes
        .par_iter_mut()
        .zip(paths)
        .filter(|(change, _)| change.is_none())
        .for_each(|(change, path)| {
            *change = fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| Timestamp::try_from(modified).ok())
                .map(LastChange::Modified);
        });
    changes
}

/// Groups the files of `paths` that are inside git repositories by repository root, the
/// nearest directory above a file with a `.git`. Each directory is resolved once, and files
/// outside repositories cost no `git` process. A file's own name is kept unresolved, as git
/// tracks a symbolic link by its name.
fn group_by_repository(paths: &[&Path]) -> HashMap<PathBuf, Vec<TrackedFile>> {
    let Ok(current) = env::current_dir() else {
        return HashMap::new();
    };
    let mut roots: HashMap<&Path, Option<(PathBuf, Vec<u8>)>> = HashMap::new();
    let mut known_roots = HashMap::new();
    let mut repositories: HashMap<PathBuf, Vec<TrackedFile>> = HashMap::new();
    for (index, path) in paths.iter().enumerate() {
        let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
            continue;
        };
        let located = roots.entry(directory).or_insert_with(|| {
            let absolute = absolute_directory(&current, directory)?;
            let root = repository_root(&absolute, &mut known_roots)?;
            let prefix = git_path(absolute.strip_prefix(&root).ok()?);
            Some((root, prefix))
        });
        let Some((root, prefix)) = located else {
            continue;
        };
        let mut relative = Vec::with_capacity(prefix.len() + name.len() + 1);
        relative.extend_from_slice(prefix);
        if !prefix.is_empty() {
            relative.push(b'/');
        }
        relative.extend_from_slice(name.as_encoded_bytes());
        // `git log --stdin` reads one path per line.
        if !relative.contains(&b'\n') {
            repositories
                .entry(root.clone())
                .or_default()
                .push((index, relative));
        }
    }
    repositories
}

/// `directory` as an absolute path without `.` components. Only a path that steps up with
/// `..` needs the file system to resolve it, as the directory it names depends on symbolic
/// links; `git` takes any other path as written, through links or not.
fn absolute_directory(current: &Path, directory: &Path) -> Option<PathBuf> {
    let joined = current.join(directory);
    if joined
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return fs::canonicalize(joined).ok();
    }
    Some(joined.components().collect())
}

/// The root of the repository holding the absolute `directory`: its nearest ancestor with a
/// `.git`. The answer is remembered in `known` for every directory checked on the way, so
/// directories that share ancestors check each of them once.
fn repository_root(
    directory: &Path,
    known: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> Option<PathBuf> {
    let mut checked = Vec::new();
    let mut root = None;
    for ancestor in directory.ancestors() {
        if let Some(known_root) = known.get(ancestor) {
            root = known_root.clone();
            break;
        }
        checked.push(ancestor);
        if ancestor.join(".git").exists() {
            root = Some(ancestor.to_path_buf());
            break;
        }
    }
    for directory in checked {
        known.insert(directory.to_path_buf(), root.clone());
    }
    root
}

/// `relative` as `git` prints it: components joined by `/` on every platform.
fn git_path(relative: &Path) -> Vec<u8> {
    let mut joined = Vec::new();
    for (index, component) in relative.iter().enumerate() {
        if index > 0 {
            joined.push(b'/');
        }
        joined.extend_from_slice(component.as_encoded_bytes());
    }
    joined
}

/// Records the last commit of each of `files` in the repository at `root` into `changes`, by
/// reading one `git log` limited to those files until every file has been seen. Untracked
/// files, and every file when `root` is no real repository, stay `None`.
fn commit_times(root: &Path, files: &[TrackedFile], changes: &mut [Option<LastChange>]) {
    let mut pending: HashMap<&[u8], Vec<usize>> = HashMap::with_capacity(files.len());
    for (index, relative) in files {
        pending.entry(relative).or_default().push(*index);
    }
    let spawned = Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            // Paths come on stdin, after a `--` line, so any number of them fit.
            "--stdin",
            "--no-show-signature",
            // A merge that changed a file is its last change, like `git log -1 -- file`.
            "--diff-merges=first-parent",
            "--format=%x01%ct",
            "--name-only",
            "-z",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        GIT_MISSING.store(true, Ordering::Relaxed);
        return;
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };
    let mut paths = Vec::from(*b"--\n");
    for relative in pending.keys() {
        paths.extend_from_slice(relative);
        paths.push(b'\n');
    }
    // `git` reads every path before it prints, so writing them all first can't deadlock.
    // A failed write means `git` exited, as outside a real repository; it then prints nothing.
    let _ = stdin.write_all(&paths);
    drop(stdin);
    // Records are NUL-terminated: a commit's mark and time, then the files it changed, the
    // first of them after a line break.
    let mut reader = BufReader::new(stdout);
    let mut field = Vec::new();
    let mut commit_time = None;
    while !pending.is_empty() && reader.read_until(0, &mut field).is_ok_and(|read| read > 0) {
        let text = field.strip_suffix(&[0]).unwrap_or(&field);
        if let Some(seconds) = text.strip_prefix(&[COMMIT_MARK]) {
            commit_time = std::str::from_utf8(seconds)
                .ok()
                .and_then(|seconds| seconds.parse().ok())
                .and_then(|seconds| Timestamp::from_second(seconds).ok());
        } else if let Some(time) = commit_time {
            let name = text.strip_prefix(b"\n").unwrap_or(text);
            for index in pending.remove(name).into_iter().flatten() {
                changes[index] = Some(LastChange::Committed(time));
            }
        }
        field.clear();
    }
    // Every file is found, so the rest of the history is not needed; `git` may have exited.
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    use jiff::tz::Offset;

    use super::*;

    /// 2026-03-18 10:15:00 UTC.
    const SECONDS: i64 = 1_773_828_900;

    fn timestamp(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).expect("the time is in range")
    }

    #[test]
    fn changes_are_described_to_the_minute_in_the_given_zone() {
        let pacific = TimeZone::fixed(Offset::constant(-7));

        assert_eq!(
            LastChange::Committed(timestamp(SECONDS)).describe(&pacific),
            "committed 2026-03-18 03:15"
        );
        assert_eq!(
            LastChange::Modified(timestamp(SECONDS)).describe(&TimeZone::UTC),
            "modified 2026-03-18 10:15"
        );
    }

    #[test]
    fn changes_of_either_kind_order_by_their_time() {
        let earlier = LastChange::Committed(timestamp(SECONDS));
        let later = LastChange::Modified(timestamp(SECONDS + 1));

        assert!(earlier.time() < later.time());
        assert_eq!(later.time(), timestamp(SECONDS + 1));
        assert_eq!(earlier.minute(), later.minute());
        assert_eq!(LastChange::Modified(timestamp(-1)).minute(), -1);
    }

    #[test]
    fn repository_roots_are_found_once_for_directories_sharing_ancestors() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = fs::canonicalize(root.path()).expect("the directory resolves");
        fs::create_dir_all(repository.join(".git")).expect("the directory should be created");
        fs::create_dir_all(repository.join("a/b")).expect("the directory should be created");
        let outside = repository
            .parent()
            .expect("temporary directories have parents");
        let mut known = HashMap::new();

        let nested = repository_root(&repository.join("a/b"), &mut known);
        let sibling = repository_root(&repository.join("a"), &mut known);

        assert_eq!(nested.as_deref(), Some(repository.as_path()));
        assert_eq!(sibling.as_deref(), Some(repository.as_path()));
        assert_eq!(known.len(), 3);
        assert_ne!(
            repository_root(outside, &mut known).as_deref(),
            Some(repository.as_path())
        );
    }

    #[test]
    fn directories_become_absolute_and_resolve_only_steps_up() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let current = fs::canonicalize(root.path()).expect("the directory resolves");
        fs::create_dir_all(current.join("a/b")).expect("the directory should be created");

        let joined = |directory: &str| absolute_directory(&current, Path::new(directory));

        assert_eq!(joined(""), Some(current.clone()));
        assert_eq!(joined("./a/b"), Some(current.join("a/b")));
        assert_eq!(joined("a/b/.."), Some(current.join("a")));
        assert_eq!(joined("missing/.."), None);
    }

    #[test]
    fn git_paths_join_components_with_slashes() {
        assert_eq!(git_path(Path::new("src/bin/main.rs")), b"src/bin/main.rs");
        assert_eq!(git_path(Path::new("lib.rs")), b"lib.rs");
    }

    #[test]
    fn files_outside_git_report_their_modification_time() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let path = root.path().join("notes.txt");
        let file = File::create(&path).expect("the file should be created");
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(SECONDS as u64);
        file.set_modified(modified)
            .expect("the modification time should be set");

        let changes = last_changes(&[&path, &root.path().join("missing.txt")]);

        assert_eq!(
            changes,
            [Some(LastChange::Modified(timestamp(SECONDS))), None]
        );
    }

    #[test]
    fn committed_files_report_their_last_commit_in_one_batch() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let git = |seconds: i64, arguments: &[&str]| {
            Command::new("git")
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(arguments)
                .current_dir(root.path())
                .env("GIT_COMMITTER_DATE", format!("@{seconds} +0000"))
                .env("GIT_AUTHOR_DATE", format!("@{seconds} +0000"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        if !git(SECONDS, &["init", "-q"]) {
            // Without git, every file reports its modification time instead.
            return;
        }
        let write = |relative: &str| {
            fs::write(root.path().join(relative), relative).expect("the file is written");
        };
        fs::create_dir(root.path().join("src")).expect("the directory should be created");
        write("src/lib.rs");
        write("src/old file.rs");
        assert!(git(SECONDS, &["add", "src"]));
        assert!(git(SECONDS, &["commit", "-q", "-m", "Add both"]));
        fs::write(root.path().join("src/lib.rs"), "changed").expect("the file is written");
        assert!(git(SECONDS + 60, &["commit", "-q", "-am", "Change lib"]));
        write("draft.rs");
        let lib = root.path().join("src/lib.rs");
        let old = root.path().join("src/old file.rs");
        let draft = root.path().join("draft.rs");

        let changes = last_changes(&[&lib, &old, &draft, &lib]);

        assert_eq!(
            changes[..3],
            [
                Some(LastChange::Committed(timestamp(SECONDS + 60))),
                Some(LastChange::Committed(timestamp(SECONDS))),
                changes[2],
            ]
        );
        assert!(matches!(changes[2], Some(LastChange::Modified(_))));
        assert_eq!(changes[3], changes[0]);
    }
}
