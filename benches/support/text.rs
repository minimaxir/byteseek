//! A synthetic multilingual UTF-8 text and its scenarios, for comparing how the command-line
//! tools search text that is mostly not ASCII.

use byteseek::CaseMode;

use crate::scenarios::Scenario;

/// Lines in the text from [`build_utf8_corpus`].
pub const UTF8_LINE_COUNT: usize = 200_000;

/// Words in Cyrillic, Greek, Japanese, accented Latin and plain ASCII, so most bytes are
/// multibyte sequences and pattern bytes recur in unmatched text.
const WORDS: [&str; 20] = [
    "жёлтый",
    "квадрат",
    "синий",
    "круг",
    "дорога",
    "κύκλος",
    "δρόμος",
    "θάλασσα",
    "東京",
    "タワー",
    "電車",
    "café",
    "müller",
    "straße",
    "naïve",
    "status",
    "event",
    "user",
    "handled",
    "request",
];

/// Phrases that make lines match, with the spacing of the lines that hold them.
const PHRASES: [(&str, usize); 4] = [
    ("Жёлтый Квадрат", 997),
    ("東京タワー", 1499),
    ("Café Müller", 701),
    ("status=503", 211),
];

const SINGLE_NON_ASCII: [&str; 1] = ["Жёлтый Квадрат"];
const MULTI_NON_ASCII: [&str; 3] = ["Жёлтый Квадрат", "東京タワー", "Café Müller"];
// ASCII letters fold; `ü` matches only itself.
const MULTI_NON_ASCII_INSENSITIVE: [&str; 3] = ["café müller", "STATUS=503", "東京タワー"];
const SINGLE_ASCII: [&str; 1] = ["status=503"];
const MULTI_WHOLE_WORD: [&str; 3] = ["Café Müller", "status=503", "東京タワー"];

/// Scenarios run on the text from [`build_utf8_corpus`].
pub const UTF8_SCENARIOS: [Scenario; 5] = [
    Scenario::new("utf8_single", &SINGLE_NON_ASCII, CaseMode::Sensitive),
    Scenario::new("utf8_multi", &MULTI_NON_ASCII, CaseMode::Sensitive),
    Scenario::new(
        "utf8_multi_ascii_insensitive",
        &MULTI_NON_ASCII_INSENSITIVE,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new("utf8_single_ascii", &SINGLE_ASCII, CaseMode::Sensitive),
    Scenario {
        whole_word: true,
        ..Scenario::new(
            "utf8_multi_whole_word",
            &MULTI_WHOLE_WORD,
            CaseMode::Sensitive,
        )
    },
];

/// Builds a deterministic text of `lines` lines of one to ten words each, some holding one
/// of [`PHRASES`].
pub fn build_utf8_corpus(lines: usize) -> String {
    let mut corpus = String::with_capacity(lines * 64);
    // A linear congruential generator keeps the text the same on every run and platform.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = |bound: usize| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // The high bits are the most random; the bound is far below 2^32.
        (state >> 33) as usize % bound
    };
    for index in 0..lines {
        let words = 1 + next(10);
        for word in 0..words {
            if word > 0 {
                corpus.push(' ');
            }
            corpus.push_str(WORDS[next(WORDS.len())]);
        }
        for (phrase, spacing) in PHRASES {
            if index % spacing == 0 {
                corpus.push(' ');
                corpus.push_str(phrase);
            }
        }
        corpus.push('\n');
    }
    corpus
}
