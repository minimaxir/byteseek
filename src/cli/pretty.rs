//! `--pretty`: a terminal layout where weight and color carry meaning. Paths head their
//! matches, selected lines have a gutter in the color of the pattern that matched, context
//! and metadata are faint, and a footer sums up the search.

use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use memchr::memchr_iter;
use unicode_width::UnicodeWidthChar;

use crate::cli::changed::LastChange;
use crate::cli::style::{
    self, AMBER, BOLD, Depth, FAINT, GRADIENT, Ink, PLAIN, RED, colored, gradient_at,
};
use crate::cli::walk::STDIN_NAME;
use crate::reporting::{append_byte_size, decimal_len};
use crate::{Match, Searcher, StructuredMatch, StructuredMatchKind};

/// The gutter beside selected rows; context rows leave it blank, so they stand apart without
/// color.
const GUTTER: &str = "\u{258e}";
/// Marks skipped lines between groups of context.
const GAP: &str = "\u{22ee}";
const ELLIPSIS: &str = "\u{2026}";
const SEPARATOR: &str = " \u{00b7} ";
/// Columns a tab takes.
const TAB_WIDTH: usize = 4;
/// Largest share of the width the path column of structured rows takes.
const PATH_SHARE: usize = 2;

/// Totals of a search, shared by the threads that search files.
#[derive(Debug, Default)]
pub struct Tally {
    files: AtomicUsize,
    bytes: AtomicU64,
    matched_files: AtomicUsize,
    lines: AtomicUsize,
    matches: AtomicUsize,
    errors: AtomicUsize,
    per_pattern: Box<[AtomicUsize]>,
}

impl Tally {
    /// Creates a tally for `patterns` patterns.
    pub fn new(patterns: usize) -> Self {
        Self {
            per_pattern: (0..patterns).map(|_| AtomicUsize::new(0)).collect(),
            ..Self::default()
        }
    }

    /// Records a searched file of `bytes` bytes.
    pub fn searched(&self, bytes: usize) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Records a file or directory that could not be searched.
    pub fn failed(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a file's `lines` selected lines and the patterns of its matches.
    pub fn found(&self, lines: usize, patterns: impl Iterator<Item = usize>) {
        if lines == 0 {
            return;
        }
        self.matched_files.fetch_add(1, Ordering::Relaxed);
        self.lines.fetch_add(lines, Ordering::Relaxed);
        let mut matches = 0;
        for pattern in patterns {
            matches += 1;
            if let Some(count) = self.per_pattern.get(pattern) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.matches.fetch_add(matches, Ordering::Relaxed);
    }
}

/// A row of a file's output, in line order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    /// Lines were skipped.
    Gap,
    /// A context line: one-based number and text.
    Context(usize, &'a str),
    /// A selected line: one-based number, text, and matches as offsets into the text.
    Selected(usize, &'a str, &'a [Match]),
}

/// The `--pretty` renderer.
#[derive(Debug, Clone, Copy)]
pub struct Pretty<'a> {
    /// Color depth of stdout.
    pub depth: Depth,
    /// Terminal columns, if stdout is a terminal; longer lines are trimmed to fit.
    pub width: Option<usize>,
    /// Number of patterns, which share the gradient between them.
    pub patterns: usize,
    /// The patterns without structured parsing, to find where a structured value matched.
    pub values: Option<&'a Searcher>,
}

/// Display columns of `text` as rendered: tabs expanded, control characters as one symbol.
fn columns(text: &str) -> usize {
    text.chars().map(char_columns).sum()
}

fn char_columns(character: char) -> usize {
    match character {
        '\t' => TAB_WIDTH,
        _ if character.is_control() => 1,
        _ => character.width().unwrap_or(0),
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// `bytes` in binary units, like `3.2 MB`.
fn byte_size(bytes: u64) -> String {
    let mut size = String::new();
    append_byte_size(&mut size, bytes);
    size
}

fn elapsed(duration: Duration) -> String {
    let millis = duration.as_secs_f64() * 1000.0;
    if millis < 10.0 {
        format!("{millis:.1} ms")
    } else if millis < 1000.0 {
        format!("{millis:.0} ms")
    } else {
        format!("{:.2} s", millis / 1000.0)
    }
}

/// This machine's name, which file URLs carry so a terminal can tell a file on another
/// machine, as over SSH, from a local one.
static HOST: LazyLock<String> =
    LazyLock::new(|| gethostname::gethostname().to_string_lossy().into_owned());

/// `path` on `host` as a `file://` URL, percent-encoding all but unreserved bytes and
/// separators.
fn file_url(host: &str, path: &Path) -> Option<String> {
    let absolute = std::path::absolute(path).ok()?;
    let mut url = String::from("file://");
    for &byte in host
        .as_bytes()
        .iter()
        .chain(absolute.as_os_str().as_encoded_bytes())
    {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(url)
}

/// Collapses runs of whitespace, so a multi-line value fits on one row.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Pretty<'_> {
    fn paint(&self, out: &mut Vec<u8>, ink: Ink, text: &str) {
        style::paint(out, self.depth, ink, text.as_bytes());
    }

    /// The color of pattern `index`: patterns spread evenly over the gradient.
    pub fn pattern_color(&self, index: usize) -> u32 {
        if self.patterns <= 1 {
            GRADIENT[0]
        } else {
            gradient_at(index as f32 / (self.patterns - 1) as f32)
        }
    }

    /// Writes `path` with a faint directory and a bold name, as a terminal hyperlink when
    /// colors are on.
    fn path(&self, out: &mut Vec<u8>, path: &[u8]) {
        let text = String::from_utf8_lossy(path);
        let url = (self.depth != Depth::None && path != STDIN_NAME)
            .then(|| file_url(&HOST, Path::new(text.as_ref())))
            .flatten();
        if let Some(url) = &url {
            out.extend_from_slice(format!("\x1b]8;;{url}\x1b\\").as_bytes());
        }
        let (directory, name) = text
            .rsplit_once('/')
            .map_or(("", text.as_ref()), |(dir, name)| {
                (&text[..dir.len() + 1], name)
            });
        self.paint(out, FAINT, directory);
        self.paint(out, BOLD, name);
        if url.is_some() {
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
    }

    /// Writes a file's heading: its path, then faint counts.
    fn header(&self, out: &mut Vec<u8>, path: &[u8], counts: &[String]) {
        self.path(out, path);
        self.paint(out, FAINT, &format!("  {}", counts.join(SEPARATOR)));
        out.push(b'\n');
    }

    /// Writes when a file last changed, to end its heading.
    pub fn last_change(&self, out: &mut Vec<u8>, change: LastChange) {
        self.paint(
            out,
            FAINT,
            &format!("{SEPARATOR}{}", change.describe_local()),
        );
    }

    /// Writes the line-number column and the gutter that start a row, blank without `gutter`.
    fn margin(&self, out: &mut Vec<u8>, width: usize, number: Option<usize>, gutter: Option<Ink>) {
        let number = number.map_or_else(String::new, |number| number.to_string());
        self.paint(out, FAINT, &format!("{number:>width$} "));
        match gutter {
            Some(ink) => self.paint(out, ink, GUTTER),
            None => out.push(b' '),
        }
        out.push(b' ');
    }

    /// Columns left for text after a margin of a `width`-digit number column.
    fn room(&self, width: usize) -> Option<usize> {
        self.width
            .map(|columns| columns.saturating_sub(width + 3).max(8))
    }

    /// Writes `text` in `base` ink with `spans` (sorted byte ranges) in their own inks,
    /// showing control characters as symbols. Text wider than `room` is trimmed to a window
    /// that starts at the line's start when the matches allow, else covers as many matches
    /// as fit; an ellipsis that hides a match takes its color.
    fn text(
        &self,
        out: &mut Vec<u8>,
        text: &str,
        spans: &[(usize, usize, Ink)],
        base: Ink,
        room: Option<usize>,
    ) {
        let characters: Vec<(usize, char)> = text.char_indices().collect();
        // `offsets[i]` is the column where character `i` starts.
        let offsets: Vec<usize> = std::iter::once(0)
            .chain(characters.iter().scan(0, |column, &(_, character)| {
                *column += char_columns(character);
                Some(*column)
            }))
            .collect();
        let width = |from: usize, to: usize| offsets[to] - offsets[from];
        let index_of = |byte: usize| characters.partition_point(|&(start, _)| start < byte);
        let (mut start, mut end) = (0, characters.len());
        if let Some(room) = room.filter(|&room| width(0, characters.len()) > room) {
            let first = spans.first().map_or(0, |span| index_of(span.0));
            let last = spans
                .iter()
                .map(|span| index_of(span.1))
                .max()
                .unwrap_or(first);
            let fits = |from: usize, to: usize| width(from, to) + 2 <= room;
            if !fits(0, last) {
                // Keep up to a third of the room before the first match, less if that pushes
                // later matches out of view.
                start = first;
                while start > 0
                    && width(start - 1, first) <= room / 3
                    && fits(start - 1, last.max(first + 1))
                {
                    start -= 1;
                }
                if start == first {
                    while start > 0 && width(start - 1, first) <= room / 3 && !fits(first, last) {
                        start -= 1;
                    }
                }
            }
            let budget = room - usize::from(start > 0) - 1;
            end = start;
            while end < characters.len() && width(start, end + 1) <= budget {
                end += 1;
            }
        }
        let (from_byte, to_byte) = (
            characters.get(start).map_or(text.len(), |&(byte, _)| byte),
            characters.get(end).map_or(text.len(), |&(byte, _)| byte),
        );
        let hidden_ink = |hidden: Option<&(usize, usize, Ink)>| hidden.map_or(FAINT, |span| span.2);
        if start > 0 {
            let hidden = spans.iter().find(|span| span.0 < from_byte);
            self.paint(out, hidden_ink(hidden), ELLIPSIS);
        }
        let mut next_span = spans.partition_point(|span| span.1 <= from_byte);
        let mut run = String::with_capacity(to_byte - from_byte);
        let mut run_ink = base;
        for &(byte, character) in &characters[start..end] {
            while spans.get(next_span).is_some_and(|span| span.1 <= byte) {
                next_span += 1;
            }
            let picture = match u32::from(character) {
                0x09 => None,
                code @ 0..0x20 => char::from_u32(0x2400 + code),
                0x7f => Some('\u{2421}'),
                _ if character.is_control() => Some('\u{fffd}'),
                _ => None,
            };
            let ink = match spans.get(next_span) {
                _ if picture.is_some() => FAINT,
                Some(span) if span.0 <= byte => span.2,
                Some(_) | None => base,
            };
            if ink != run_ink && !run.is_empty() {
                self.paint(out, run_ink, &run);
                run.clear();
            }
            run_ink = ink;
            match picture {
                Some(picture) => run.push(picture),
                None if character == '\t' => run.extend(std::iter::repeat_n(' ', TAB_WIDTH)),
                None => run.push(character),
            }
        }
        self.paint(out, run_ink, &run);
        if end < characters.len() {
            let hidden = spans.iter().find(|span| span.1 > to_byte);
            self.paint(out, hidden_ink(hidden), ELLIPSIS);
        }
    }

    /// The highlight spans of `matches`, sorted and without overlaps.
    fn match_spans(&self, matches: &[Match]) -> Vec<(usize, usize, Ink)> {
        let mut sorted = matches.to_vec();
        sorted.sort_unstable_by_key(|span| (span.start, span.end));
        let mut spans: Vec<(usize, usize, Ink)> = Vec::with_capacity(sorted.len());
        for span in sorted {
            match spans.last_mut() {
                Some(last) if span.start < last.1 => last.1 = last.1.max(span.end),
                Some(_) | None => spans.push((
                    span.start,
                    span.end,
                    colored(self.pattern_color(span.pattern_index)).bold(),
                )),
            }
        }
        spans
    }

    /// Writes a file's selected lines, their context, and gaps under a heading.
    pub fn lines(&self, out: &mut Vec<u8>, path: &[u8], rows: &[Row]) {
        let (mut lines, mut matches, mut last) = (0, 0, 0);
        for row in rows {
            match row {
                Row::Selected(number, _, found) => {
                    lines += 1;
                    matches += found.len();
                    last = *number;
                }
                Row::Context(number, _) => last = *number,
                Row::Gap => {}
            }
        }
        let mut counts = vec![plural(lines, "line", "lines")];
        if matches > lines {
            counts.push(plural(matches, "match", "matches"));
        }
        self.header(out, path, &counts);
        let width = decimal_len(last);
        let room = self.room(width);
        for row in rows {
            match *row {
                Row::Gap => {
                    self.paint(out, FAINT, &format!("{:>width$} {GAP}", ""));
                    out.push(b'\n');
                }
                Row::Context(number, text) => {
                    self.margin(out, width, Some(number), None);
                    self.text(out, text, &[], FAINT, room);
                    out.push(b'\n');
                }
                Row::Selected(number, text, found) => {
                    // Lines selected by `-v` or `NOT` alone have no match to take a color from.
                    let gutter = found
                        .iter()
                        .min_by_key(|span| span.start)
                        .map_or(PLAIN, |span| {
                            colored(self.pattern_color(span.pattern_index))
                        });
                    self.margin(out, width, Some(number), Some(gutter));
                    self.text(out, text, &self.match_spans(found), PLAIN, room);
                    out.push(b'\n');
                }
            }
        }
    }

    /// Writes each match on its own row under a heading.
    pub fn only_matches(&self, out: &mut Vec<u8>, path: &[u8], rows: &[(usize, &str, &[Match])]) {
        let matches: usize = rows.iter().map(|row| row.2.len()).sum();
        self.header(out, path, &[plural(matches, "match", "matches")]);
        let width = decimal_len(rows.last().map_or(0, |row| row.0));
        for &(number, text, found) in rows {
            for (index, span) in found.iter().enumerate() {
                let ink = colored(self.pattern_color(span.pattern_index));
                self.margin(out, width, (index == 0).then_some(number), Some(ink));
                let piece = String::from_utf8_lossy(&text.as_bytes()[span.start..span.end]);
                self.text(out, &piece, &[], ink.bold(), self.room(width));
                out.push(b'\n');
            }
        }
    }

    /// Writes a jq path with faint punctuation and keys and indexes in `key` ink.
    fn json_path(&self, out: &mut Vec<u8>, path: &str, key: Ink) {
        let mut rest = path;
        while !rest.is_empty() {
            let split = rest
                .char_indices()
                .skip(1)
                .find(|&(_, character)| matches!(character, '.' | '['))
                .map_or(rest.len(), |(index, _)| index);
            let (segment, tail) = rest.split_at(split);
            if let Some(index) = segment.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                self.paint(out, FAINT, "[");
                self.paint(out, key, index);
                self.paint(out, FAINT, "]");
            } else if let Some(name) = segment.strip_prefix('.') {
                self.paint(out, FAINT, ".");
                self.paint(out, key, name);
            } else {
                self.paint(out, FAINT, segment);
            }
            rest = tail;
        }
    }

    /// The ink of a structured value: faint for null and containers, whose scalars print on
    /// rows of their own, else plain, as color is kept for patterns.
    fn value_ink(value: &str) -> Ink {
        if matches!(value, "null" | "~") || value.starts_with(['{', '[']) {
            FAINT
        } else {
            PLAIN
        }
    }

    /// Writes a file's structured matches as aligned `path  value` rows under a heading.
    pub fn structured(
        &self,
        out: &mut Vec<u8>,
        path: &[u8],
        text: &str,
        found: &[StructuredMatch],
    ) {
        self.header(out, path, &[plural(found.len(), "match", "matches")]);
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(memchr_iter(b'\n', text.as_bytes()).map(|index| index + 1))
            .collect();
        let line_of = |offset: usize| line_starts.partition_point(|&start| start <= offset);
        let width = decimal_len(
            found
                .iter()
                .map(|item| line_of(item.start))
                .max()
                .unwrap_or(1),
        );
        let path_cap = self
            .width
            .map_or(usize::MAX, |columns| columns / PATH_SHARE);
        let path_width = found
            .iter()
            .map(|item| columns(&item.path))
            .filter(|&path_columns| path_columns <= path_cap)
            .max()
            .unwrap_or(0);
        let mut rows: Vec<&StructuredMatch> = found.iter().collect();
        rows.sort_by_key(|item| (item.start, item.pattern_index));
        for item in rows {
            let (path_matched, value_matched) = match item.match_kind {
                StructuredMatchKind::Path => (true, false),
                StructuredMatchKind::Value => (false, true),
                StructuredMatchKind::PathValue | StructuredMatchKind::Predicate => (true, true),
            };
            let ink = colored(self.pattern_color(item.pattern_index));
            self.margin(out, width, Some(line_of(item.start)), Some(ink));
            self.json_path(
                out,
                &item.path,
                if path_matched { ink.bold() } else { PLAIN },
            );
            let used = columns(&item.path);
            let pad = path_width.saturating_sub(used) + 2;
            out.extend(std::iter::repeat_n(b' ', pad));
            let value = one_line(item.node_text);
            let base = Self::value_ink(&value);
            let spans = match (item.match_kind, self.values) {
                (StructuredMatchKind::Value, Some(values)) => {
                    let mut found = values.find(&value);
                    found.retain(|span| span.pattern_index == item.pattern_index);
                    self.match_spans(&found)
                }
                _ => Vec::new(),
            };
            // Predicates and `path=value` patterns select the whole value.
            let base = if value_matched && spans.is_empty() {
                base.bold()
            } else {
                base
            };
            let room = self
                .room(width)
                .map(|room| room.saturating_sub(used + pad).max(8));
            self.text(out, &value, &spans, base, room);
            out.push(b'\n');
        }
    }

    /// Writes a file's count of selected lines, or of matches when `matches` is set, beside
    /// its path.
    pub fn count(&self, out: &mut Vec<u8>, path: &[u8], count: usize, matches: bool) {
        self.path(out, path);
        out.extend_from_slice(b"  ");
        let counted = if matches {
            plural(count, "match", "matches")
        } else {
            plural(count, "line", "lines")
        };
        self.figure(out, &counted);
        out.push(b'\n');
    }

    /// Writes a matching file's path.
    pub fn file(&self, out: &mut Vec<u8>, path: &[u8]) {
        self.path(out, path);
        out.push(b'\n');
    }

    /// Writes `text`, a number then its words, with the number bold and the words faint.
    fn figure(&self, out: &mut Vec<u8>, text: &str) {
        let (number, words) = text.split_once(' ').unwrap_or((text, ""));
        self.paint(out, BOLD, number);
        self.paint(out, FAINT, &format!(" {words}"));
    }

    /// Writes the closing summary: totals, then each pattern's matches in its color when
    /// there are several and any matched.
    pub fn summary(
        &self,
        out: &mut Vec<u8>,
        tally: &Tally,
        patterns: &[String],
        took: Duration,
        structured: bool,
    ) {
        let load = |count: &AtomicUsize| count.load(Ordering::Relaxed);
        let files = load(&tally.files);
        let matched = load(&tally.matched_files);
        let (lines, matches) = (load(&tally.lines), load(&tally.matches));
        let errors = load(&tally.errors);
        let (one, many) = if structured {
            ("document", "documents")
        } else {
            ("file", "files")
        };
        let mut totals = Vec::with_capacity(3);
        if matches > 0 {
            totals.push(plural(matches, "match", "matches"));
        }
        if !structured && lines > 0 && lines != matches {
            totals.push(plural(lines, "line", "lines"));
        }
        totals.push(format!("{matched} of {}", plural(files, one, many)));
        if totals.len() == 1 {
            self.paint(out, colored(AMBER).bold(), "no matches");
            self.paint(out, FAINT, SEPARATOR);
        }
        // Errors go before the file total, which is short of the files that failed.
        let (file_total, counts) = totals
            .split_last()
            .expect("the file total is always pushed");
        for total in counts {
            self.figure(out, total);
            self.paint(out, FAINT, SEPARATOR);
        }
        if errors > 0 {
            self.paint(out, colored(RED).bold(), &plural(errors, "error", "errors"));
            self.paint(out, FAINT, SEPARATOR);
        }
        self.figure(out, file_total);
        self.paint(out, FAINT, SEPARATOR);
        self.figure(out, &byte_size(tally.bytes.load(Ordering::Relaxed)));
        self.paint(out, FAINT, " in ");
        self.figure(out, &elapsed(took));
        out.push(b'\n');
        // A legend of zeros, as after `-v`, says nothing that "no matches" does not.
        if patterns.len() > 1 && matches > 0 {
            for (index, pattern) in patterns.iter().enumerate() {
                let color = colored(self.pattern_color(index));
                self.paint(out, color, GUTTER);
                out.push(b' ');
                self.paint(out, color.bold(), pattern);
                let count = tally.per_pattern.get(index).map_or(0, load);
                self.paint(out, FAINT, &format!(" {count}"));
                if index + 1 < patterns.len() {
                    out.extend_from_slice(b"   ");
                }
            }
            out.push(b'\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNCOLORED: Pretty<'static> = Pretty {
        depth: Depth::None,
        width: None,
        patterns: 2,
        values: None,
    };

    fn span(start: usize, end: usize, pattern_index: usize) -> Match {
        Match {
            start,
            end,
            pattern_index,
        }
    }

    fn render(write: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        write(&mut out);
        String::from_utf8(out).expect("output should be UTF-8")
    }

    #[test]
    fn lines_align_numbers_and_mark_gaps() {
        let first = [span(2, 5, 0)];
        let second = [span(0, 3, 1)];
        let rows = [
            Row::Context(1, "a"),
            Row::Selected(2, "b hit", &first),
            Row::Gap,
            Row::Selected(10, "hit\tx", &second),
        ];

        let output = render(|out| UNCOLORED.lines(out, b"f.txt", &rows));

        assert_eq!(
            output,
            "f.txt  2 lines\n 1   a\n 2 \u{258e} b hit\n   \u{22ee}\n10 \u{258e} hit    x\n"
        );
    }

    #[test]
    fn only_matches_number_each_line_once() {
        let found = [span(0, 3, 0), span(4, 7, 1)];

        let output = render(|out| UNCOLORED.only_matches(out, b"f", &[(3, "hit hit", &found)]));

        assert_eq!(output, "f  2 matches\n3 \u{258e} hit\n  \u{258e} hit\n");
    }

    #[test]
    fn long_text_is_trimmed_around_its_match() {
        let text = format!("{} hit {}", "a".repeat(30), "b".repeat(16));
        let spans = [(31, 34, BOLD)];

        let output = render(|out| UNCOLORED.text(out, &text, &spans, PLAIN, Some(12)));

        assert_eq!(output, "\u{2026}aaa hit bb\u{2026}");
    }

    #[test]
    fn control_characters_show_as_pictures() {
        let output = render(|out| UNCOLORED.text(out, "a\u{1}b\u{7f}\u{85}", &[], PLAIN, None));

        assert_eq!(output, "a\u{2401}b\u{2421}\u{fffd}");
    }

    #[test]
    fn structured_rows_follow_the_document_with_aligned_values() {
        let text = "{\n  \"a\": 1,\n  \"long\": \"x\"\n}";
        let item = |start: usize, path: &str, kind| StructuredMatch {
            start,
            end: start + 1,
            pattern_index: 0,
            path: path.to_owned(),
            node_text: &text[start
                ..text[start..]
                    .find(['\n', ','])
                    .map_or(text.len(), |end| start + end)],
            match_kind: kind,
        };
        let found = [
            item(22, "$.long", StructuredMatchKind::Value),
            item(9, "$.a", StructuredMatchKind::Path),
        ];

        let output = render(|out| UNCOLORED.structured(out, b"d.json", text, &found));

        assert_eq!(
            output,
            "d.json  2 matches\n2 \u{258e} $.a     1\n3 \u{258e} $.long  \"x\"\n"
        );
    }

    #[test]
    fn summary_totals_the_search_and_each_pattern() {
        let tally = Tally::new(2);
        tally.searched(10);
        tally.searched(2000);
        tally.searched(5);
        tally.found(2, [0, 1, 1].into_iter());
        tally.found(0, std::iter::empty());
        let took = Duration::from_micros(1500);
        let patterns = ["x".to_owned(), "y".to_owned()];

        let failed = Tally::new(1);
        failed.failed();

        let output = render(|out| UNCOLORED.summary(out, &tally, &patterns, took, false));
        let unmatched =
            render(|out| UNCOLORED.summary(out, &Tally::new(2), &patterns, took, false));
        let empty = render(|out| UNCOLORED.summary(out, &failed, &[], took, true));

        assert_eq!(
            output,
            "3 matches \u{b7} 2 lines \u{b7} 1 of 3 files \u{b7} 2.0 KB in 1.5 ms\n\
             \u{258e} x 1   \u{258e} y 2\n"
        );
        assert_eq!(
            unmatched,
            "no matches \u{b7} 0 of 0 files \u{b7} 0 B in 1.5 ms\n"
        );
        assert_eq!(
            empty,
            "no matches \u{b7} 1 error \u{b7} 0 of 0 documents \u{b7} 0 B in 1.5 ms\n"
        );
    }

    #[test]
    fn counts_name_their_unit() {
        let output = render(|out| {
            UNCOLORED.count(out, b"a", 1, false);
            UNCOLORED.count(out, b"b", 3, true);
        });

        assert_eq!(output, "a  1 line\nb  3 matches\n");
    }

    #[test]
    fn colored_paths_link_to_their_files() {
        let pretty = Pretty {
            depth: Depth::TrueColor,
            ..UNCOLORED
        };

        let link = file_url(&HOST, Path::new("dir/f.rs")).expect("the path is absolutized");

        let output = render(|out| pretty.path(out, b"dir/f.rs"));

        assert!(output.starts_with(&format!("\x1b]8;;{link}\x1b\\")));
        assert!(output.ends_with("\x1b[2mdir/\x1b[0m\x1b[1mf.rs\x1b[0m\x1b]8;;\x1b\\"));
    }

    #[test]
    fn patterns_span_the_gradient() {
        assert_eq!(UNCOLORED.pattern_color(0), GRADIENT[0]);
        assert_eq!(UNCOLORED.pattern_color(1), GRADIENT[3]);
    }

    #[test]
    fn sizes_times_and_urls_are_formatted() {
        assert_eq!(byte_size(512), "512 B");
        assert_eq!(byte_size(1536), "1.5 KB");
        assert_eq!(byte_size(3 << 20), "3.0 MB");
        assert_eq!(elapsed(Duration::from_millis(25)), "25 ms");
        assert_eq!(elapsed(Duration::from_millis(1500)), "1.50 s");
        assert_eq!(
            file_url("", Path::new("/tmp/a b%.rs")).as_deref(),
            Some("file:///tmp/a%20b%25.rs")
        );
        assert_eq!(
            file_url("box.local", Path::new("/a")).as_deref(),
            Some("file://box.local/a")
        );
        assert_eq!(plural(1, "match", "matches"), "1 match");
        assert_eq!(one_line("a\n  b"), "a b");
    }
}
