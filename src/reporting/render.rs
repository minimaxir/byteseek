use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::path::Path;

use smallvec::SmallVec;

use super::aggregation::DisplayRoot;
use super::{
    DirectoryHotspot, FileHotspot, PatternComparisonRow, PatternFrequency, SearchReportStats,
};

/// Padding source for [`append_run`].
const SPACES: &str = "                                                                ";
/// A run of dashes for table borders.
const DASHES: &str = "----------------------------------------------------------------";
/// Capacity reserved for a formatted title, whose length is unknown until written.
const FORMATTED_TITLE_LEN: usize = 64;
/// The bar drawn for the directory with the most matches.
const DENSITY_BAR: &str = "########################";

/// Renders a terminal summary of scan statistics.
pub(super) fn format_summary(title: &str, stats: SearchReportStats) -> String {
    // Upper bound: four 20-digit counts, an 11-byte size, and the fixed text.
    let mut output = String::with_capacity(title.len() + 184);
    let mut digits = itoa::Buffer::new();
    output.push_str(title);
    output.push_str("\nScanned: ");
    output.push_str(digits.format(stats.scanned_files));
    output.push_str(" files, ");
    append_byte_size(&mut output, stats.scanned_bytes);
    output.push_str("\nMatched: ");
    output.push_str(digits.format(stats.matched_files));
    output.push_str(" files, ");
    output.push_str(digits.format(stats.total_matches));
    output.push_str(" matches, ");
    output.push_str(digits.format(stats.total_matching_lines));
    output.push_str(" matching lines");
    output
}

/// Appends a byte count in binary units (`1023 B`, `1.5 KB`, `10 KB`), rounding half to even
/// in integer arithmetic exactly as the equivalent `f64` formatting would.
pub(crate) fn append_byte_size(output: &mut String, bytes: u64) {
    const UNITS: [&str; 4] = [" KB", " MB", " GB", " TB"];

    let mut digits = itoa::Buffer::new();
    if bytes < 1024 {
        output.push_str(digits.format(bytes));
        output.push_str(" B");
        return;
    }
    // Round through `f64` first so huge counts match the floating-point formatting exactly.
    let value = (bytes as f64) as u128;
    let unit = ((value.ilog2() / 10) as usize).min(UNITS.len());
    let shift = 10 * unit;
    let whole = value >= 10 << shift;
    let scaled = if whole { value } else { value * 10 };
    let (mut quotient, remainder, half) = (
        scaled >> shift,
        scaled & ((1 << shift) - 1),
        1_u128 << (shift - 1),
    );
    if remainder > half || (remainder == half && quotient % 2 == 1) {
        quotient += 1;
    }
    if whole {
        output.push_str(digits.format(quotient));
    } else {
        output.push_str(digits.format(quotient / 10));
        output.push('.');
        output.push_str(digits.format(quotient % 10));
    }
    output.push_str(UNITS[unit - 1]);
}

/// Renders rows of one text label followed by `N` count columns, without materializing cells.
pub(super) fn render_counted_table<'r, const N: usize>(
    title: fmt::Arguments<'_>,
    headers: &[&str],
    rows: impl ExactSizeIterator<Item = (&'r str, [usize; N])> + Clone,
) -> String {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<SmallVec<[usize; 4]>>();
    for (label, counts) in rows.clone() {
        widths[0] = widths[0].max(label.len());
        for (width, count) in widths[1..].iter_mut().zip(counts) {
            *width = (*width).max(decimal_len(count));
        }
    }

    render_table_with(
        title,
        headers,
        &mut widths,
        rows,
        |cells, (label, counts)| {
            cells.text(label);
            for count in counts {
                cells.count("", count);
            }
        },
    )
}

pub(super) fn render_file_table(
    root: DisplayRoot<'_>,
    hotspots: &[FileHotspot],
    limit: usize,
) -> String {
    let rows = &hotspots[..hotspots.len().min(limit)];
    let labels = rows
        .iter()
        .map(|entry| root.label(entry.path.as_path()))
        .collect::<RowLabels<'_>>();
    let mut widths = [
        "File".len(),
        "Matches".len(),
        "Lines".len(),
        "First Byte".len(),
    ];

    for (entry, label) in rows.iter().zip(labels.iter()) {
        widths[0] = widths[0].max(label.len());
        widths[1] = widths[1].max(decimal_len(entry.match_count));
        widths[2] = widths[2].max(decimal_len(entry.matching_lines));
        widths[3] = widths[3].max(decimal_len(entry.first_match_byte));
    }

    render_table_with(
        format_args!("Top Matching Files"),
        &["File", "Matches", "Lines", "First Byte"],
        &mut widths,
        rows.iter().zip(labels.iter()),
        |cells, (entry, label)| {
            cells.text(label);
            cells.count("", entry.match_count);
            cells.count("", entry.matching_lines);
            cells.count("", entry.first_match_byte);
        },
    )
}

/// Hotspot paths are already relative to the scanned root, so they are not stripped again.
pub(super) fn render_directory_density(hotspots: &[DirectoryHotspot], limit: usize) -> String {
    if limit == 0 || hotspots.is_empty() {
        return "Directory Match Density\nNo matching directories.".to_owned();
    }

    let rows = &hotspots[..hotspots.len().min(limit)];
    let max_count = rows[0].match_count.max(1);
    let labels = rows
        .iter()
        .map(|entry| display_label(entry.path.as_path()))
        .collect::<RowLabels<'_>>();
    let width = labels.iter().map(|label| label.len()).max().unwrap_or(1);

    // Room for every row plus the whole runs that `append_run` copies before cutting.
    let mut output = Vec::with_capacity(24 + rows.len() * (width + 44) + SPACES.len());
    output.extend_from_slice(b"Directory Match Density\n");
    for (entry, label) in rows.iter().zip(labels.iter()) {
        let bar_len = entry
            .match_count
            .saturating_mul(DENSITY_BAR.len())
            .div_ceil(max_count);
        let mut count_buffer = itoa::Buffer::new();
        let count = count_buffer.format(entry.match_count);
        output.extend_from_slice(label.as_bytes());
        append_run(
            &mut output,
            SPACES,
            width - label.len() + 2 + 6_usize.saturating_sub(count.len()),
        );
        output.extend_from_slice(count.as_bytes());
        output.extend_from_slice(b" matches  ");
        append_run(&mut output, DENSITY_BAR, bar_len.max(1));
        output.push(b'\n');
    }
    output.pop();
    into_text(output)
}

pub(super) fn render_pattern_table(patterns: &[PatternFrequency]) -> String {
    let mut widths = [
        "Pattern".len(),
        "Text".len(),
        "Matches".len(),
        "Files".len(),
    ];
    for entry in patterns {
        widths[0] = widths[0].max(1 + decimal_len(entry.pattern_index));
        widths[1] = widths[1].max(entry.pattern.len());
        widths[2] = widths[2].max(decimal_len(entry.match_count));
        widths[3] = widths[3].max(decimal_len(entry.matched_files));
    }

    render_table_with(
        format_args!("Pattern Frequencies"),
        &["Pattern", "Text", "Matches", "Files"],
        &mut widths,
        patterns.iter(),
        |cells, entry| {
            cells.count("#", entry.pattern_index);
            cells.text(entry.pattern.as_str());
            cells.count("", entry.match_count);
            cells.count("", entry.matched_files);
        },
    )
}

pub(super) fn render_pattern_comparison_table(
    root: DisplayRoot<'_>,
    rows: &[PatternComparisonRow],
    patterns: &[PatternFrequency],
    file_limit: usize,
    pattern_limit: usize,
) -> String {
    if file_limit == 0 || pattern_limit == 0 || rows.is_empty() || patterns.is_empty() {
        return "Multi-Pattern File Comparison\nNo matched files or patterns to compare."
            .to_owned();
    }

    let selected_patterns = active_patterns(patterns, pattern_limit);
    if selected_patterns.is_empty() {
        return "Multi-Pattern File Comparison\nNo matched files or patterns to compare."
            .to_owned();
    }
    let shown_rows = &rows[..rows.len().min(file_limit)];

    let mut headers = Vec::with_capacity(selected_patterns.len() + 2);
    headers.push("File");
    headers.extend(
        selected_patterns
            .iter()
            .map(|pattern| pattern.pattern.as_str()),
    );
    headers.push("Total");

    let mut totals = vec![0_usize; selected_patterns.len()];
    let mut rendered_rows = shown_rows
        .iter()
        .map(|entry| {
            let mut row = Vec::with_capacity(selected_patterns.len() + 2);
            row.push(root.label(entry.path.as_path()).into_owned());

            for (column_index, pattern) in selected_patterns.iter().enumerate() {
                let count = entry.pattern_counts[pattern.pattern_index];
                totals[column_index] += count;
                row.push(count.to_string());
            }

            row.push(entry.total_matches.to_string());
            row
        })
        .collect::<Vec<_>>();

    let shown_total_matches = shown_rows
        .iter()
        .map(|entry| entry.total_matches)
        .sum::<usize>();
    let mut total_row = Vec::with_capacity(selected_patterns.len() + 2);
    total_row.push(String::from("SHOWN TOTAL"));
    total_row.extend(totals.into_iter().map(|count| count.to_string()));
    total_row.push(shown_total_matches.to_string());
    rendered_rows.push(total_row);

    render_table(
        format_args!("Multi-Pattern File Comparison"),
        headers.as_slice(),
        &rendered_rows,
    )
}

pub(super) fn render_pattern_cooccurrence_matrix(
    rows: &[PatternComparisonRow],
    patterns: &[PatternFrequency],
    pattern_limit: usize,
) -> String {
    if pattern_limit == 0 || rows.is_empty() || patterns.is_empty() {
        return "Pattern File Co-occurrence\nNo matched files or patterns to compare.".to_owned();
    }

    let selected_patterns = active_patterns(patterns, pattern_limit);
    if selected_patterns.is_empty() {
        return "Pattern File Co-occurrence\nNo matched files or patterns to compare.".to_owned();
    }
    let mut headers = Vec::with_capacity(selected_patterns.len() + 1);
    headers.push("Pattern");
    headers.extend(
        selected_patterns
            .iter()
            .map(|pattern| pattern.pattern.as_str()),
    );

    let rendered_rows = selected_patterns
        .iter()
        .map(|row_pattern| {
            let row_files = row_pattern.matched_files;
            let mut row = Vec::with_capacity(selected_patterns.len() + 1);
            row.push(row_pattern.pattern.clone());

            for column_pattern in selected_patterns {
                let shared_files = rows
                    .iter()
                    .filter(|entry| {
                        entry.pattern_counts[row_pattern.pattern_index] > 0
                            && entry.pattern_counts[column_pattern.pattern_index] > 0
                    })
                    .count();
                row.push(format!("{shared_files}/{row_files}"));
            }

            row
        })
        .collect::<Vec<_>>();

    render_table(
        format_args!("Pattern File Co-occurrence (shared files / row files)"),
        headers.as_slice(),
        &rendered_rows,
    )
}

fn active_patterns(patterns: &[PatternFrequency], limit: usize) -> &[PatternFrequency] {
    let active_count = patterns.partition_point(|pattern| pattern.match_count > 0);
    &patterns[..active_count.min(limit)]
}

/// Row labels, stored inline for the usual top-ten tables.
type RowLabels<'a> = SmallVec<[Cow<'a, str>; 16]>;

/// Same text as `Path::display`, borrowed whenever the path is valid UTF-8.
pub(super) fn display_label(path: &Path) -> Cow<'_, str> {
    // `to_str` is faster than `to_string_lossy`, which walks the path chunk by chunk.
    path.to_str()
        .map_or_else(|| path.to_string_lossy(), Cow::Borrowed)
}

/// Renders `rows` beneath `headers` as an ASCII table. Cell widths are measured in bytes.
pub(super) fn render_table<T: AsRef<str>>(
    title: fmt::Arguments<'_>,
    headers: &[T],
    rows: &[Vec<String>],
) -> String {
    let mut widths = headers
        .iter()
        .map(|header| header.as_ref().len())
        .collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.len());
        }
    }

    render_table_with(title, headers, &mut widths, rows.iter(), |cells, row| {
        for cell in row {
            cells.text(cell);
        }
    })
}

/// One blank table row whose columns are filled left to right.
#[derive(Debug)]
struct RowCells<'t> {
    line: &'t mut [u8],
    /// Offset in `line` of each column's text.
    columns: &'t [usize],
    column: usize,
}

impl RowCells<'_> {
    /// Writes `text` into the next column, whose width the caller sized to fit it.
    ///
    /// # Panics
    ///
    /// Panics when the text runs past the row's end or the row has no further column.
    #[inline]
    fn text(&mut self, text: &str) {
        let start = self.columns[self.column];
        self.line[start..start + text.len()].copy_from_slice(text.as_bytes());
        self.column += 1;
    }

    /// Writes `tag` followed by the decimal digits of `value` into the next column.
    ///
    /// # Panics
    ///
    /// Panics when they run past the row's end or the row has no further column.
    #[inline]
    fn count(&mut self, tag: &str, value: usize) {
        let mut buffer = itoa::Buffer::new();
        let digits = buffer.format(value);
        let start = self.columns[self.column];
        let (tagged, rest) = self.line[start..].split_at_mut(tag.len());
        tagged.copy_from_slice(tag.as_bytes());
        rest[..digits.len()].copy_from_slice(digits.as_bytes());
        self.column += 1;
    }
}

/// Renders a table whose data rows `fill` writes cell by cell.
///
/// One border and one blank row are drawn from `widths`; every other line is a copy of one of
/// them with cells written into the blanks, so padding costs one copy per line.
#[inline]
fn render_table_with<T: AsRef<str>, Rows: ExactSizeIterator>(
    title: fmt::Arguments<'_>,
    headers: &[T],
    widths: &mut [usize],
    rows: Rows,
    mut fill: impl FnMut(&mut RowCells<'_>, Rows::Item),
) -> String {
    let row_count = rows.len();
    if row_count == 0
        && let Some(first) = widths.first_mut()
    {
        *first = (*first).max("No data".len());
    }
    // Text starts two bytes into a column: after its separator and a space.
    let mut line_len = 2;
    let columns = widths
        .iter()
        .map(|width| {
            let start = line_len;
            line_len += width + 3;
            start
        })
        .collect::<SmallVec<[usize; 8]>>();
    // Every line plus the slack `append_run` overwrites before truncating.
    let table_len = line_len * (row_count.max(1) + 4) + SPACES.len();
    // `as_str` is `Some` for a title without placeholders.
    let fixed_title = title.as_str();
    let mut text =
        String::with_capacity(fixed_title.map_or(FORMATTED_TITLE_LEN, str::len) + 1 + table_len);
    match fixed_title {
        Some(fixed_title) => text.push_str(fixed_title),
        None => text
            .write_fmt(title)
            .expect("formatting into a string cannot fail"),
    }
    text.push('\n');
    let mut output = text.into_bytes();
    output.reserve(table_len);
    let border = append_ruled_line(&mut output, widths, b'+', DASHES);
    // The header line stays blank as the template for rows until they are written.
    let blank = append_ruled_line(&mut output, widths, b'|', SPACES);
    output.extend_from_within(border.clone());

    let next_row = |output: &mut Vec<u8>| {
        let start = output.len();
        output.extend_from_within(blank.clone());
        start
    };
    // A table without columns has no cell for the notice.
    if row_count == 0
        && let Some(&first_column) = columns.first()
    {
        let start = next_row(&mut output);
        output[start + first_column..][.."No data".len()].copy_from_slice(b"No data");
    }
    for row in rows {
        let start = next_row(&mut output);
        let mut cells = RowCells {
            line: &mut output[start..],
            columns: &columns,
            column: 0,
        };
        fill(&mut cells, row);
    }
    let mut header_cells = RowCells {
        line: &mut output[blank.clone()],
        columns: &columns,
        column: 0,
    };
    for header in headers {
        header_cells.text(header.as_ref());
    }

    // The closing border ends the table without a line break.
    output.extend_from_within(border.start..border.end - 1);
    into_text(output)
}

/// Appends a border or blank line built from `separator` and `fill`, returning its range in
/// `output` (line break included).
#[inline(always)]
fn append_ruled_line(
    output: &mut Vec<u8>,
    widths: &[usize],
    separator: u8,
    fill: &str,
) -> std::ops::Range<usize> {
    let start = output.len();
    output.push(separator);
    for &width in widths {
        append_run(output, fill, width + 2);
        output.push(separator);
    }
    output.push(b'\n');
    start..output.len()
}

/// Appends `count` bytes of `run`, a repeated ASCII character, repeating it as needed.
///
/// The last piece is copied whole and then truncated: with a constant `run` that is a few
/// vector stores instead of a variable-length `memcpy` call.
#[inline(always)]
fn append_run(output: &mut Vec<u8>, run: &str, mut count: usize) {
    while count > run.len() {
        output.extend_from_slice(run.as_bytes());
        count -= run.len();
    }
    let end = output.len() + count;
    output.extend_from_slice(run.as_bytes());
    output.truncate(end);
}

/// Renderers only copy whole strings into disjoint ranges, so the bytes stay valid UTF-8.
fn into_text(rendered: Vec<u8>) -> String {
    String::from_utf8(rendered).expect("renderers write whole strings without overlap")
}

/// The digits of `value` in decimal.
pub(crate) fn decimal_len(value: usize) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_and_generic_tables_preserve_exact_output() {
        let files = [FileHotspot {
            path: "a".into(),
            match_count: 12,
            matching_lines: 3,
            first_match_byte: 10,
        }];
        let file_root = DisplayRoot::with_kind(Path::new(""), false);
        let headers = ["File", "Matches", "Lines", "First Byte"];
        let rows = vec![["a", "12", "3", "10"].map(str::to_owned).to_vec()];
        let expected = "Top Matching Files\n\
+------+---------+-------+------------+\n\
| File | Matches | Lines | First Byte |\n\
+------+---------+-------+------------+\n\
| a    | 12      | 3     | 10         |\n\
+------+---------+-------+------------+";
        for table in [
            render_file_table(file_root, &files, 1),
            render_table(format_args!("Top Matching Files"), &headers, &rows),
            render_table(
                format_args!("Top Matching Files"),
                &headers.map(str::to_owned),
                &rows,
            ),
        ] {
            assert_eq!(table, expected);
        }
        assert_eq!(
            render_table(format_args!("Empty"), &["A"], &[]),
            "Empty\n+---------+\n| A       |\n+---------+\n| No data |\n+---------+"
        );
        assert!(render_file_table(file_root, &files, 0).contains("| No data |"));
        let repeated_files = [files[0].clone(), files[0].clone()];
        let repeated_rows = [rows[0].clone(), rows[0].clone()];
        assert_eq!(
            render_file_table(file_root, &repeated_files, 2),
            render_table(format_args!("Top Matching Files"), &headers, &repeated_rows)
        );
        for value in [0, 9, 10, 99, 100, usize::MAX] {
            assert_eq!(decimal_len(value), value.to_string().len());
        }
    }

    #[test]
    fn direct_tables_match_materialized_cells() {
        let patterns = [
            PatternFrequency {
                pattern_index: 10,
                pattern: "needle".to_owned(),
                match_count: 12_345,
                matched_files: 7,
            },
            PatternFrequency {
                pattern_index: 2,
                pattern: "a".to_owned(),
                match_count: 0,
                matched_files: 0,
            },
        ];
        let rows = patterns
            .iter()
            .map(|entry| {
                vec![
                    format!("#{}", entry.pattern_index),
                    entry.pattern.clone(),
                    entry.match_count.to_string(),
                    entry.matched_files.to_string(),
                ]
            })
            .collect::<Vec<_>>();
        let headers = ["Pattern", "Text", "Matches", "Files"];
        assert_eq!(
            render_pattern_table(&patterns),
            render_table(format_args!("Pattern Frequencies"), &headers, &rows)
        );
        assert_eq!(
            render_pattern_table(&[]),
            render_table(format_args!("Pattern Frequencies"), &headers, &[])
        );

        let counted = [("$.path.to.value", [3_usize, 1]), ("$.x", [1_000_000, 22])];
        let counted_rows = counted
            .iter()
            .map(|(label, [matches, files])| {
                vec![(*label).to_owned(), matches.to_string(), files.to_string()]
            })
            .collect::<Vec<_>>();
        let counted_headers = ["Path", "Matches", "Files"];
        assert_eq!(
            render_counted_table(
                format_args!("Paths"),
                &counted_headers,
                counted.iter().copied()
            ),
            render_table(format_args!("Paths"), &counted_headers, &counted_rows)
        );
    }

    #[test]
    fn summary_matches_formatted_text() {
        let stats = SearchReportStats {
            scanned_files: 3,
            scanned_bytes: 1536,
            matched_files: 2,
            total_matches: 1_234,
            total_matching_lines: 99,
        };
        assert_eq!(
            format_summary("Title", stats),
            "Title\nScanned: 3 files, 1.5 KB\nMatched: 2 files, 1234 matches, 99 matching lines"
        );
        let sizes = [0_u64, 1023, 1536, 10 * 1024, 5 << 30, u64::MAX].map(byte_size);
        assert_eq!(
            sizes,
            ["0 B", "1023 B", "1.5 KB", "10 KB", "5.0 GB", "16777216 TB"]
        );
    }

    fn byte_size(bytes: u64) -> String {
        let mut text = String::new();
        append_byte_size(&mut text, bytes);
        text
    }

    /// The floating-point formatting that integer byte sizes must reproduce.
    fn float_byte_size(bytes: u64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB"];
        let mut unit = 0;
        let mut scaled = bytes as f64;
        while scaled >= 1024.0 && unit + 1 < units.len() {
            scaled /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else if scaled >= 10.0 {
            format!("{scaled:.0} {}", units[unit])
        } else {
            format!("{scaled:.1} {}", units[unit])
        }
    }

    #[test]
    fn integer_byte_sizes_round_like_floating_point() {
        for bytes in 0..=(2_u64 << 20) {
            assert_eq!(byte_size(bytes), float_byte_size(bytes), "{bytes}");
        }
        // Neighbors of powers of two, rounding ties, and where `f64` loses integer precision.
        for shift in 10..64 {
            let power = 1_u64 << shift;
            for scale in [1, 3, 5, 9, 10, 11, 19, 21, 1023] {
                let Some(base) = power.checked_mul(scale) else {
                    continue;
                };
                for bytes in [
                    base / 2 - 1,
                    base / 2,
                    base / 2 + 1,
                    base - 1,
                    base,
                    base + 1,
                ] {
                    assert_eq!(byte_size(bytes), float_byte_size(bytes), "{bytes}");
                }
            }
        }
        for bytes in [
            (1 << 53) - 1,
            1 << 53,
            (1 << 53) + 1,
            u64::MAX - 1024,
            u64::MAX,
        ] {
            assert_eq!(byte_size(bytes), float_byte_size(bytes), "{bytes}");
        }
    }

    #[test]
    fn padding_runs_longer_than_their_source_text_stay_exact() {
        for count in [0, 1, 23, 24, 25, 63, 64, 65, 128, 200] {
            let mut bar = b"x".to_vec();
            append_run(&mut bar, DENSITY_BAR, count);
            assert_eq!(into_text(bar), format!("x{}", "#".repeat(count)));
        }
        let wide = "w".repeat(150);
        let rows = vec![vec![wide.clone(), "1".to_owned()]];
        let table = render_table(format_args!("Wide"), &["Name", "Count"], &rows);
        let border = format!("+{}+-------+", "-".repeat(152));
        assert_eq!(
            table,
            format!(
                "Wide\n{border}\n| Name{} | Count |\n{border}\n| {wide} | 1     |\n{border}",
                " ".repeat(146)
            )
        );
    }

    #[test]
    fn tables_keep_multibyte_cells() {
        let rows = vec![vec!["caf\u{e9}".to_owned(), "\u{65e5}\u{672c}".to_owned()]];
        assert_eq!(
            render_table(format_args!("Part two"), &["A", "B"], &rows),
            "Part two\n+-------+--------+\n| A     | B      |\n+-------+--------+\n\
| caf\u{e9} | \u{65e5}\u{672c} |\n+-------+--------+"
        );
    }
}
