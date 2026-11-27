use byteseek::{CaseMode, SearchConfig};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink};

/// Builds a literal matcher with `config`'s ASCII case and whole-word settings.
pub fn build_matcher(patterns: &[&str], config: SearchConfig) -> RegexMatcher {
    RegexMatcherBuilder::new()
        .case_insensitive(config.case_mode() == CaseMode::AsciiInsensitive)
        .word(config.whole_word())
        .line_terminator(Some(b'\n'))
        .unicode(false)
        .build_literals(patterns)
        .expect("ripgrep literal matcher should compile")
}

/// Builds an in-memory text searcher without line-number reporting.
pub fn build_searcher() -> Searcher {
    SearcherBuilder::new().line_number(false).build()
}

#[derive(Default)]
struct CountSink {
    count: usize,
}

impl Sink for CountSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        _matched: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.count += 1;
        Ok(true)
    }
}

/// Counts matching lines in `corpus` with ripgrep.
pub fn count_matching_lines(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    corpus: &str,
) -> usize {
    let mut sink = CountSink::default();
    searcher
        .search_slice(matcher, corpus.as_bytes(), &mut sink)
        .expect("ripgrep search should succeed");
    sink.count
}

/// Whether `line[start..end]` is bounded by non-word ASCII bytes.
fn is_whole_word(line: &[u8], start: usize, end: usize) -> bool {
    let is_word_byte = |byte: &u8| byte.is_ascii_alphanumeric() || *byte == b'_';
    let before = start.checked_sub(1).map(|index| &line[index]);
    !before.is_some_and(is_word_byte) && !line.get(end).is_some_and(is_word_byte)
}

/// Naive, independent count of lines containing any of `patterns`, using `config`'s ASCII
/// case and whole-word settings. Patterns must be nonempty and contain no line terminator.
pub fn oracle_count(patterns: &[&str], config: SearchConfig, corpus: &str) -> usize {
    assert!(!patterns.is_empty());
    assert!(patterns.iter().all(|pattern| {
        !pattern.is_empty()
            && !pattern
                .as_bytes()
                .iter()
                .any(|byte| matches!(byte, b'\r' | b'\n'))
    }));
    corpus
        .lines()
        .filter(|line| {
            patterns.iter().any(|pattern| {
                let line = line.as_bytes();
                line.windows(pattern.len())
                    .enumerate()
                    .any(|(start, window)| {
                        let equal = match config.case_mode() {
                            CaseMode::Sensitive => window == pattern.as_bytes(),
                            CaseMode::AsciiInsensitive => {
                                window.eq_ignore_ascii_case(pattern.as_bytes())
                            }
                        };
                        equal
                            && (!config.whole_word()
                                || is_whole_word(line, start, start + pattern.len()))
                    })
            })
        })
        .count()
}
