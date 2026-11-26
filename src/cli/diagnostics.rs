//! Errors and warnings on stderr, labeled `error:` and `warning:` as clap labels usage errors.

use std::fmt;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Formats each event as `label: message`, with a bold colored label when `color` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diagnostics {
    /// Write ANSI escapes.
    pub color: bool,
}

impl Diagnostics {
    /// Writes the label of an event at `level`.
    fn label(self, writer: &mut impl fmt::Write, level: Level) -> fmt::Result {
        let (label, color) = match level {
            Level::ERROR => ("error", 31),
            Level::WARN => ("warning", 33),
            _ => ("note", 36),
        };
        if self.color {
            write!(writer, "\x1b[1;{color}m{label}\x1b[0m\x1b[1m:\x1b[0m ")
        } else {
            write!(writer, "{label}: ")
        }
    }
}

impl<S, N> FormatEvent<S, N> for Diagnostics
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        self.label(&mut writer, *event.metadata().level())?;
        context
            .field_format()
            .format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(color: bool, level: Level) -> String {
        let mut out = String::new();
        Diagnostics { color }
            .label(&mut out, level)
            .expect("writing to a string succeeds");
        out
    }

    #[test]
    fn labels_name_the_level_and_color_only_on_request() {
        assert_eq!(label(false, Level::ERROR), "error: ");
        assert_eq!(label(false, Level::WARN), "warning: ");
        assert_eq!(
            label(true, Level::ERROR),
            "\x1b[1;31merror\x1b[0m\x1b[1m:\x1b[0m "
        );
    }
}
