//! Colors and text weights for `--pretty`, written as ANSI escapes at the depth the terminal
//! supports.

use std::env;

/// How many colors the output can show; `None` writes no escapes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Plain text.
    None,
    /// The xterm 256-color palette.
    EightBit,
    /// 24-bit color.
    TrueColor,
}

/// A text run's look: 24-bit color (`None` for the terminal's own), bold, and faint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ink {
    /// Foreground color as `0xRRGGBB`.
    pub color: Option<u32>,
    /// Heavier weight, for what was searched for.
    pub bold: bool,
    /// Lighter weight, for metadata and context.
    pub faint: bool,
}

/// The terminal's own text.
pub const PLAIN: Ink = Ink {
    color: None,
    bold: false,
    faint: false,
};
/// Metadata, context, and punctuation.
pub const FAINT: Ink = Ink {
    faint: true,
    ..PLAIN
};
/// What the eye should land on first.
pub const BOLD: Ink = Ink {
    bold: true,
    ..PLAIN
};

/// Colored text at regular weight.
pub const fn colored(color: u32) -> Ink {
    Ink {
        color: Some(color),
        ..PLAIN
    }
}

impl Ink {
    /// The same ink, bold.
    pub const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }
}

/// Stops of the gradient patterns are colored from, each with 3:1 contrast on both light and
/// dark backgrounds.
pub const GRADIENT: [u32; 4] = [0xA467E0, 0x4C7EBC, 0x1F8D9D, 0x1B976F];
/// Amber, for searches that came back empty.
pub const AMBER: u32 = 0xBA7D0A;
/// Red, for files that could not be searched.
pub const RED: u32 = 0xD8475B;

/// Variables set by CI services and AI coding agents, whose logs show escapes as noise.
const PLAIN_ENVIRONMENTS: [&str; 19] = [
    "CI",
    "CONTINUOUS_INTEGRATION",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "BUILDKITE",
    "JENKINS_URL",
    "CIRCLECI",
    "TF_BUILD",
    "AI_AGENT",
    "CLAUDECODE",
    "CODEX_SANDBOX",
    "CURSOR_AGENT",
    "GEMINI_CLI",
    "QWEN_CODE",
    "CLINE_ACTIVE",
    "COPILOT_AGENT",
    "OPENCODE",
    "AUGMENT_AGENT",
    "AMP_CURRENT_THREAD_ID",
];

/// Whether variable `name` is set to anything but an empty string, 0, false, no, or off.
fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn no_color() -> bool {
    env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

/// Whether `TERM` allows color: it must be set to anything but `dumb`, except on Windows,
/// where it is often unset.
fn term_allows_color() -> bool {
    env::var_os("TERM").map_or(cfg!(windows), |term| term != "dumb")
}

/// ripgrep's `--color auto`: color on a terminal unless `NO_COLOR` is present, even empty,
/// or `TERM` forbids it.
pub fn auto_color(terminal: bool) -> bool {
    terminal && env::var_os("NO_COLOR").is_none() && term_allows_color()
}

/// Chooses the color depth of a stream, `terminal` saying whether it is one. `NO_COLOR` and
/// `never` turn color off; `always`, `FORCE_COLOR`, and `CLICOLOR_FORCE` turn it on; otherwise
/// color needs a terminal outside CI and AI agents, without `CLICOLOR=0`.
pub fn detect(always: bool, never: bool, terminal: bool) -> Depth {
    if never || no_color() {
        return Depth::None;
    }
    let wanted = always
        || env_flag("FORCE_COLOR")
        || env_flag("CLICOLOR_FORCE")
        || (terminal
            && term_allows_color()
            && env::var("CLICOLOR").map_or(true, |value| value != "0")
            && !PLAIN_ENVIRONMENTS.iter().any(|name| env_flag(name)));
    if !wanted {
        return Depth::None;
    }
    let program = env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term = env::var("TERM").unwrap_or_default();
    let true_color = matches!(
        env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "truecolor" | "24bit"
    ) || env::var("FORCE_COLOR").is_ok_and(|level| level == "3")
        || env::var_os("WT_SESSION").is_some()
        || matches!(
            program.as_str(),
            "iterm.app" | "wezterm" | "ghostty" | "vscode" | "warpterminal"
        )
        || [
            "xterm-kitty",
            "xterm-ghostty",
            "alacritty",
            "foot",
            "contour",
        ]
        .iter()
        .any(|prefix| term.starts_with(prefix));
    if true_color {
        Depth::TrueColor
    } else {
        Depth::EightBit
    }
}

fn channels(color: u32) -> [u32; 3] {
    [(color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF]
}

/// The color `amount` of the way from `start` to `end`, for `amount` in `[0, 1]`.
fn blend(start: u32, end: u32, amount: f32) -> u32 {
    let [start, end] = [channels(start), channels(end)];
    (0..3).fold(0, |color, channel| {
        let low = start[channel] as f32;
        let high = end[channel] as f32;
        (color << 8) | (low + (high - low) * amount).round() as u32
    })
}

/// [`GRADIENT`] sampled at `position` in `[0, 1]`.
pub fn gradient_at(position: f32) -> u32 {
    let scaled = position.clamp(0.0, 1.0) * (GRADIENT.len() - 1) as f32;
    let index = (scaled as usize).min(GRADIENT.len() - 2);
    blend(GRADIENT[index], GRADIENT[index + 1], scaled - index as f32)
}

/// The nearest xterm 256-color index to `color`: a 6x6x6 cube entry or a gray.
fn xterm256(color: u32) -> u32 {
    const LEVELS: [u32; 6] = [0, 95, 135, 175, 215, 255];
    let rgb = channels(color);
    let cube = rgb.map(|channel| {
        (0..6)
            .min_by_key(|&index| LEVELS[index].abs_diff(channel))
            .unwrap_or(0) as u32
    });
    let gray_index = ((rgb.iter().sum::<u32>() / 3).saturating_sub(3) / 10).min(23);
    let gray = 8 + 10 * gray_index;
    let distance = |to: [u32; 3]| -> u32 {
        (0..3)
            .map(|channel| rgb[channel].abs_diff(to[channel]).pow(2))
            .sum()
    };
    let cube_rgb = cube.map(|index| LEVELS[index as usize]);
    if distance([gray; 3]) < distance(cube_rgb) {
        232 + gray_index
    } else {
        16 + 36 * cube[0] + 6 * cube[1] + cube[2]
    }
}

/// Writes `text` in `ink`, resetting the style after it.
pub fn paint(out: &mut Vec<u8>, depth: Depth, ink: Ink, text: &[u8]) {
    if depth == Depth::None || ink == PLAIN {
        out.extend_from_slice(text);
        return;
    }
    let mut codes = Vec::with_capacity(3);
    if ink.bold {
        codes.push("1".to_owned());
    }
    if ink.faint {
        codes.push("2".to_owned());
    }
    if let Some(color) = ink.color {
        codes.push(match depth {
            Depth::TrueColor => {
                let [red, green, blue] = channels(color);
                format!("38;2;{red};{green};{blue}")
            }
            Depth::EightBit => format!("38;5;{}", xterm256(color)),
            Depth::None => String::new(),
        });
    }
    out.extend_from_slice(format!("\x1b[{}m", codes.join(";")).as_bytes());
    out.extend_from_slice(text);
    out.extend_from_slice(b"\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_ends_at_its_stops() {
        assert_eq!(gradient_at(0.0), GRADIENT[0]);
        assert_eq!(gradient_at(1.0), GRADIENT[3]);
        assert_eq!(gradient_at(0.5), blend(GRADIENT[1], GRADIENT[2], 0.5));
    }

    #[test]
    fn xterm256_picks_cube_and_gray_entries() {
        assert_eq!(xterm256(0xFF0000), 196);
        assert_eq!(xterm256(0x808080), 244);
    }

    #[test]
    fn paint_writes_escapes_only_with_color() {
        let mut out = Vec::new();
        paint(&mut out, Depth::None, BOLD, b"a");
        paint(&mut out, Depth::TrueColor, colored(0x0A0B0C).bold(), b"b");
        paint(&mut out, Depth::EightBit, FAINT, b"c");

        assert_eq!(out, b"a\x1b[1;38;2;10;11;12mb\x1b[0m\x1b[2mc\x1b[0m");
    }
}
