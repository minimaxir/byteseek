//! `--report`: summary tables built by the library's reports.

use std::io::Write;
use std::path::Path;

use anyhow::{Result, bail};

use crate::Searcher;
use crate::cli::args::ReportSection;
use crate::cli::inputs::Input;

/// Renders a section that text and structured reports share, or `None` for the others.
macro_rules! common_section {
    ($report:expr, $section:expr, $top:expr) => {
        match $section {
            ReportSection::Files => Some($report.render_top_files_table($top)),
            ReportSection::Dirs => Some($report.render_directory_density($top)),
            ReportSection::Patterns => Some($report.render_pattern_frequency_table()),
            ReportSection::Compare => Some($report.render_pattern_comparison_table($top, $top)),
            ReportSection::Cooccur => Some($report.render_pattern_cooccurrence_matrix($top)),
            ReportSection::Paths | ReportSection::Values(_) => None,
        }
    };
}

/// Prints a summary and the requested tables for each input path.
///
/// # Arguments
///
/// * `out` - Where tables are written.
/// * `searcher` - The compiled patterns; structured when `--parse` was given.
/// * `inputs` - Paths to report on.
/// * `sections` - Tables to print after each summary.
/// * `top` - Rows per table.
///
/// # Returns
///
/// Whether anything matched.
///
/// # Errors
///
/// Returns an error for stdin input, structured sections without `--parse`, a path that
/// cannot be searched, or a failed write.
pub fn print(
    out: &mut impl Write,
    searcher: &Searcher,
    inputs: &[Input],
    sections: &[ReportSection],
    top: usize,
) -> Result<bool> {
    let structured = searcher.config().structured_mode().is_some();
    let mut matched = false;
    for input in inputs {
        let path: &Path = match input {
            Input::File(path) | Input::Dir(path) => path,
            Input::Stdin => bail!("--report needs files or directories, not stdin"),
        };
        let mut tables = Vec::with_capacity(sections.len() + 1);
        if structured {
            let report = searcher.structured_report_path(path)?;
            matched |= report.stats().total_matches > 0;
            tables.push(report.render_summary());
            for section in sections {
                tables.push(match section {
                    ReportSection::Paths => report.render_path_frequency_table(top),
                    ReportSection::Values(json_path) => report
                        .render_value_frequency_table(json_path, top)
                        .unwrap_or_else(|| format!("No values at {json_path}")),
                    ReportSection::Files
                    | ReportSection::Dirs
                    | ReportSection::Patterns
                    | ReportSection::Compare
                    | ReportSection::Cooccur => {
                        common_section!(report, section, top).unwrap_or_default()
                    }
                });
            }
        } else {
            let report = searcher.report_path(path)?;
            matched |= report.stats().total_matches > 0;
            tables.push(report.render_summary());
            for section in sections {
                let Some(table) = common_section!(report, section, top) else {
                    bail!("the paths and values sections need --parse");
                };
                tables.push(table);
            }
        }
        writeln!(out, "{}\n", tables.join("\n\n"))?;
    }
    Ok(matched)
}
