//! Output in ripgrep's formats: `path:line:text` for matches, `path-line-text` for context,
//! `--` between context groups, and optional headings, columns, NUL-terminated paths, and ANSI
//! colors.

use crate::Match;

const PATH_COLOR: &[u8] = b"\x1b[0m\x1b[35m";
const LINE_NUMBER_COLOR: &[u8] = b"\x1b[0m\x1b[32m";
const MATCH_COLOR: &[u8] = b"\x1b[0m\x1b[1m\x1b[31m";
// ripgrep resets around columns without coloring them.
const COLUMN_COLOR: &[u8] = b"\x1b[0m";
const RESET: &[u8] = b"\x1b[0m";

/// Separator after the path and line number of a selected line.
pub const MATCH_SEPARATOR: u8 = b':';
/// Separator after the path and line number of a context line.
pub const CONTEXT_SEPARATOR: u8 = b'-';

/// Formats one file's output into a buffer, so parallel searches print whole files at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Printer {
    /// Print paths.
    pub with_filename: bool,
    /// Print each path once above its lines instead of on every line.
    pub heading: bool,
    /// Prefix lines with their line number.
    pub line_number: bool,
    /// Prefix selected lines with the column of their first match.
    pub column: bool,
    /// End paths with a NUL byte instead of a separator or newline.
    pub null: bool,
    /// Color paths, line numbers, and matches.
    pub color: bool,
}

impl Printer {
    #[inline]
    fn colored(&self, out: &mut Vec<u8>, color: &[u8], text: &[u8]) {
        if self.color {
            out.extend_from_slice(color);
            out.extend_from_slice(text);
            out.extend_from_slice(RESET);
        } else {
            out.extend_from_slice(text);
        }
    }

    /// Writes `path` and then `terminator`, or a NUL byte with `--null`.
    #[inline]
    fn path_then(&self, out: &mut Vec<u8>, path: &[u8], terminator: u8) {
        self.colored(out, PATH_COLOR, path);
        out.push(if self.null { b'\0' } else { terminator });
    }

    /// Writes the path on its own line, before a file's first line in heading mode.
    pub fn heading(&self, out: &mut Vec<u8>, path: &[u8]) {
        if self.with_filename && self.heading {
            self.path_then(out, path, b'\n');
        }
    }

    /// Writes the path, line number, and `column` that start a line, each followed by
    /// `separator`.
    #[inline]
    fn prefix(
        &self,
        out: &mut Vec<u8>,
        path: &[u8],
        line_number: usize,
        column: Option<usize>,
        separator: u8,
    ) {
        if self.with_filename && !self.heading {
            self.path_then(out, path, separator);
        }
        let mut number = |color, value: usize| {
            self.colored(out, color, itoa::Buffer::new().format(value).as_bytes());
            out.push(separator);
        };
        if self.line_number {
            number(LINE_NUMBER_COLOR, line_number);
        }
        if let Some(column) = column.filter(|_| self.column) {
            number(COLUMN_COLOR, column);
        }
    }

    /// Whether [`Self::line`] writes lines bare, as their bytes and a line feed.
    pub fn prints_lines_bare(&self) -> bool {
        !(self.with_filename && !self.heading) && !self.line_number && !self.column && !self.color
    }

    /// Writes a whole line, highlighting `matches` (offsets into `bytes`). Context lines,
    /// written with [`CONTEXT_SEPARATOR`], have no column.
    pub fn line(
        &self,
        out: &mut Vec<u8>,
        path: &[u8],
        line_number: usize,
        bytes: &[u8],
        matches: &[Match],
        separator: u8,
    ) {
        let column = (self.column)
            .then(|| matches.iter().map(|found| found.start + 1).min())
            .flatten();
        self.prefix(out, path, line_number, column, separator);
        if self.color && !matches.is_empty() {
            let mut written = 0;
            for found in matches {
                // Overlapping matches extend the highlight instead of repeating text.
                let start = found.start.max(written);
                if found.end <= start {
                    continue;
                }
                out.extend_from_slice(&bytes[written..start]);
                self.colored(out, MATCH_COLOR, &bytes[start..found.end]);
                written = found.end;
            }
            out.extend_from_slice(&bytes[written..]);
        } else {
            out.extend_from_slice(bytes);
        }
        out.push(b'\n');
    }

    /// Writes one match, which starts at 1-based `column`, on its own line.
    pub fn only_match(
        &self,
        out: &mut Vec<u8>,
        path: &[u8],
        line_number: usize,
        column: usize,
        text: &[u8],
    ) {
        self.prefix(out, path, line_number, Some(column), MATCH_SEPARATOR);
        self.colored(out, MATCH_COLOR, text);
        out.push(b'\n');
    }

    /// Writes a structured match as `path:JQ_PATH:text`.
    pub fn structured(&self, out: &mut Vec<u8>, path: &[u8], json_path: &str, text: &str) {
        if self.with_filename && !self.heading {
            self.path_then(out, path, MATCH_SEPARATOR);
        }
        self.colored(out, LINE_NUMBER_COLOR, json_path.as_bytes());
        out.push(MATCH_SEPARATOR);
        self.colored(out, MATCH_COLOR, text.as_bytes());
        out.push(b'\n');
    }

    /// Writes the `--` that separates groups of lines with context.
    pub fn context_break(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"--\n");
    }

    /// Writes a per-file count, after the path when paths are printed.
    pub fn count(&self, out: &mut Vec<u8>, path: &[u8], count: usize) {
        if self.with_filename {
            self.path_then(out, path, MATCH_SEPARATOR);
        }
        out.extend_from_slice(itoa::Buffer::new().format(count).as_bytes());
        out.push(b'\n');
    }

    /// Writes a path on its own line.
    pub fn path(&self, out: &mut Vec<u8>, path: &[u8]) {
        self.path_then(out, path, b'\n');
    }

    /// Writes ripgrep's notice that a file with a NUL byte at `nul_offset` matched, after the
    /// lines before the NUL byte's line.
    pub fn binary_match(&self, out: &mut Vec<u8>, path: &[u8], nul_offset: usize) {
        self.binary_message(out, path, b"binary file matches", nul_offset);
    }

    /// Writes ripgrep's warning that a file's search stopped at the line of a NUL byte at
    /// `nul_offset`, after lines before it matched.
    pub fn binary_stopped(&self, out: &mut Vec<u8>, path: &[u8], nul_offset: usize) {
        let message = b"WARNING: stopped searching binary file after match";
        self.binary_message(out, path, message, nul_offset);
    }

    fn binary_message(&self, out: &mut Vec<u8>, path: &[u8], message: &[u8], nul_offset: usize) {
        if self.with_filename {
            self.colored(out, PATH_COLOR, path);
            out.extend_from_slice(b": ");
        }
        out.extend_from_slice(message);
        out.extend_from_slice(b" (found \"\\0\" byte around offset ");
        out.extend_from_slice(itoa::Buffer::new().format(nul_offset).as_bytes());
        out.extend_from_slice(b")\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Printer = Printer {
        with_filename: true,
        heading: false,
        line_number: true,
        column: false,
        null: false,
        color: false,
    };

    #[test]
    fn lines_print_bare_without_prefixes_or_colors() {
        let bare = Printer {
            with_filename: false,
            line_number: false,
            ..PLAIN
        };
        let mut line = Vec::new();
        bare.line(&mut line, b"p", 1, b"text", &[span(0, 2)], MATCH_SEPARATOR);

        assert!(bare.prints_lines_bare());
        assert_eq!(line, b"text\n");
        assert!(
            Printer {
                heading: true,
                with_filename: true,
                ..bare
            }
            .prints_lines_bare()
        );
        assert!(
            !Printer {
                with_filename: true,
                ..bare
            }
            .prints_lines_bare()
        );
        assert!(!PLAIN.prints_lines_bare());
        assert!(
            !Printer {
                color: true,
                ..bare
            }
            .prints_lines_bare()
        );
        assert!(
            !Printer {
                column: true,
                ..bare
            }
            .prints_lines_bare()
        );
    }

    fn render(write: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        write(&mut out);
        String::from_utf8(out).expect("output should be UTF-8")
    }

    fn span(start: usize, end: usize) -> Match {
        Match {
            start,
            end,
            pattern_index: 0,
        }
    }

    #[test]
    fn lines_and_context_use_their_separators() {
        let output = render(|out| {
            PLAIN.line(out, b"a.log", 3, b"ERROR x", &[span(0, 5)], MATCH_SEPARATOR);
            PLAIN.line(out, b"a.log", 4, b"next", &[], CONTEXT_SEPARATOR);
            PLAIN.context_break(out);
        });

        assert_eq!(output, "a.log:3:ERROR x\na.log-4-next\n--\n");
    }

    #[test]
    fn heading_mode_prints_the_path_once() {
        let printer = Printer {
            heading: true,
            ..PLAIN
        };

        let output = render(|out| {
            printer.heading(out, b"a.log");
            printer.line(out, b"a.log", 1, b"hit", &[span(0, 3)], MATCH_SEPARATOR);
        });

        assert_eq!(output, "a.log\n1:hit\n");
    }

    #[test]
    fn color_highlights_merged_matches() {
        let printer = Printer {
            with_filename: false,
            line_number: false,
            color: true,
            ..PLAIN
        };

        let output = render(|out| {
            printer.line(
                out,
                b"",
                1,
                b"abcd",
                &[span(0, 2), span(1, 3)],
                MATCH_SEPARATOR,
            );
        });

        assert_eq!(
            output,
            "\x1b[0m\x1b[1m\x1b[31mab\x1b[0m\x1b[0m\x1b[1m\x1b[31mc\x1b[0md\n"
        );
    }

    #[test]
    fn columns_follow_line_numbers_on_selected_lines_only() {
        let printer = Printer {
            column: true,
            ..PLAIN
        };

        let output = render(|out| {
            printer.line(
                out,
                b"a",
                2,
                b"x hit hit",
                &[span(6, 9), span(2, 5)],
                MATCH_SEPARATOR,
            );
            printer.line(out, b"a", 3, b"next", &[], CONTEXT_SEPARATOR);
            printer.only_match(out, b"a", 2, 7, b"hit");
        });

        assert_eq!(output, "a:2:3:x hit hit\na-3-next\na:2:7:hit\n");
    }

    #[test]
    fn null_ends_every_path_but_binary_notices() {
        let printer = Printer {
            null: true,
            ..PLAIN
        };
        let headed = Printer {
            heading: true,
            ..printer
        };

        let output = render(|out| {
            printer.line(out, b"a", 1, b"hit", &[span(0, 3)], MATCH_SEPARATOR);
            printer.count(out, b"b", 2);
            printer.path(out, b"c");
            headed.heading(out, b"d");
            printer.binary_match(out, b"e", 4);
        });

        assert_eq!(
            output,
            "a\x001:hit\nb\x002\nc\x00d\x00e: binary file matches (found \"\\0\" byte \
             around offset 4)\n"
        );
    }

    #[test]
    fn counts_paths_and_binary_notices() {
        let quiet_paths = Printer {
            with_filename: false,
            ..PLAIN
        };

        let output = render(|out| {
            PLAIN.count(out, b"a.log", 2);
            quiet_paths.count(out, b"a.log", 7);
            PLAIN.path(out, b"b.log");
            PLAIN.binary_match(out, b"c.bin", 12);
            quiet_paths.binary_stopped(out, b"d.bin", 5);
        });

        assert_eq!(
            output,
            "a.log:2\n7\nb.log\nc.bin: binary file matches (found \"\\0\" byte around offset \
             12)\nWARNING: stopped searching binary file after match (found \"\\0\" byte around \
             offset 5)\n"
        );
    }
}
