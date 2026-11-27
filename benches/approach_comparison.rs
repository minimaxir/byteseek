use std::hint::black_box;
use std::time::Duration;

use byteseek::{CaseMode, SearchConfig, Searcher};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    patterns: &'static [&'static str],
    case_mode: CaseMode,
}

const SENSITIVE_SINGLE: [&str; 1] = ["ERROR"];
const INSENSITIVE_SINGLE: [&str; 1] = ["warn"];
const SENSITIVE_MULTI: [&str; 4] = ["ERROR", "WARN", "status=500", "request_id=1024"];
const INSENSITIVE_MULTI: [&str; 4] = ["error", "warn", "trace", "status=500"];

const SCENARIOS: [Scenario; 4] = [
    Scenario {
        name: "single_sensitive",
        patterns: &SENSITIVE_SINGLE,
        case_mode: CaseMode::Sensitive,
    },
    Scenario {
        name: "single_ascii_insensitive",
        patterns: &INSENSITIVE_SINGLE,
        case_mode: CaseMode::AsciiInsensitive,
    },
    Scenario {
        name: "multi_sensitive",
        patterns: &SENSITIVE_MULTI,
        case_mode: CaseMode::Sensitive,
    },
    Scenario {
        name: "multi_ascii_insensitive",
        patterns: &INSENSITIVE_MULTI,
        case_mode: CaseMode::AsciiInsensitive,
    },
];

fn build_log_corpus(lines: usize) -> String {
    let mut corpus = String::with_capacity(lines * 96);
    for index in 0..lines {
        let level = match index % 8 {
            0 => "INFO",
            1 => "debug",
            2 => "WARN",
            3 => "Error",
            4 => "TRACE",
            5 => "info",
            6 => "warn",
            _ => "ERROR",
        };

        corpus.push_str("2026-01-01T00:");
        let minute = (index % 60) as u8;
        let second = ((index * 7) % 60) as u8;
        push_two_digits(&mut corpus, minute);
        corpus.push(':');
        push_two_digits(&mut corpus, second);
        corpus.push(' ');
        corpus.push_str(level);
        corpus.push_str(" service=auth request_id=");
        corpus.push_str(&index.to_string());
        corpus.push_str(" status=");
        corpus.push_str(if index % 17 == 0 { "500" } else { "200" });
        corpus.push_str(" message=handled user event\n");
    }
    corpus
}

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + (value / 10)));
    output.push(char::from(b'0' + (value % 10)));
}

struct StdPipelineMatcher<'a> {
    patterns: &'a [&'a str],
    folded_patterns: Vec<Vec<u8>>,
    case_mode: CaseMode,
}

impl<'a> StdPipelineMatcher<'a> {
    fn new(patterns: &'a [&'a str], case_mode: CaseMode) -> Self {
        let folded_patterns = if case_mode == CaseMode::AsciiInsensitive {
            patterns
                .iter()
                .map(|pattern| {
                    pattern
                        .as_bytes()
                        .iter()
                        .map(u8::to_ascii_lowercase)
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        };

        Self {
            patterns,
            folded_patterns,
            case_mode,
        }
    }

    fn count_matching_lines(&self, corpus: &str) -> usize {
        corpus.lines().filter(|line| self.is_match(line)).count()
    }

    fn is_match(&self, line: &str) -> bool {
        match self.case_mode {
            CaseMode::Sensitive => self.patterns.iter().any(|pattern| line.contains(pattern)),
            CaseMode::AsciiInsensitive => {
                let line_bytes = line.as_bytes();
                self.folded_patterns
                    .iter()
                    .any(|pattern| contains_ascii_insensitive(line_bytes, pattern.as_slice()))
            }
        }
    }
}

fn contains_ascii_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }

    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn build_grep_regex_matcher(patterns: &[&str], case_mode: CaseMode) -> RegexMatcher {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(case_mode == CaseMode::AsciiInsensitive);
    builder.unicode(false);
    builder
        .build_literals(patterns)
        .expect("literal matcher should compile")
}

fn byteseek_count_matching_lines(searcher: &Searcher, corpus: &str) -> usize {
    searcher.count_matching_lines(corpus)
}

fn grep_regex_count_matching_lines(matcher: &RegexMatcher, corpus: &str) -> usize {
    corpus
        .lines()
        .filter(|line| {
            matcher
                .is_match(line.as_bytes())
                .expect("line matching should succeed")
        })
        .count()
}

fn benchmark_approach_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("approach_comparison_single_thread_in_memory");

    for line_count in [20_000_usize, 120_000_usize] {
        let corpus = build_log_corpus(line_count);
        group.throughput(Throughput::Bytes(corpus.len() as u64));

        for scenario in SCENARIOS {
            let config = SearchConfig::builder()
                .case_mode(scenario.case_mode)
                .build();
            let rust_searcher = Searcher::new(scenario.patterns.iter().copied(), config)
                .expect("byteseek searcher should compile");
            let std_matcher = StdPipelineMatcher::new(scenario.patterns, scenario.case_mode);
            let grep_matcher = build_grep_regex_matcher(scenario.patterns, scenario.case_mode);

            let rust_count = byteseek_count_matching_lines(&rust_searcher, corpus.as_str());
            let std_count = std_matcher.count_matching_lines(corpus.as_str());
            let grep_count = grep_regex_count_matching_lines(&grep_matcher, corpus.as_str());

            assert_eq!(
                rust_count, std_count,
                "byteseek and std pipeline differ for scenario {} and line count {}",
                scenario.name, line_count
            );
            assert_eq!(
                rust_count, grep_count,
                "byteseek and grep-regex differ for scenario {} and line count {}",
                scenario.name, line_count
            );

            group.bench_with_input(
                BenchmarkId::new(format!("byteseek/{}", scenario.name), line_count),
                &corpus,
                |bench, input_corpus| {
                    bench.iter(|| {
                        black_box(byteseek_count_matching_lines(
                            black_box(&rust_searcher),
                            black_box(input_corpus.as_str()),
                        ))
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("std_pipeline/{}", scenario.name), line_count),
                &corpus,
                |bench, input_corpus| {
                    bench.iter(|| {
                        black_box(
                            std_matcher.count_matching_lines(black_box(input_corpus.as_str())),
                        )
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("grep_regex/{}", scenario.name), line_count),
                &corpus,
                |bench, input_corpus| {
                    bench.iter(|| {
                        black_box(grep_regex_count_matching_lines(
                            black_box(&grep_matcher),
                            black_box(input_corpus.as_str()),
                        ))
                    });
                },
            );
        }
    }

    group.finish();
}

fn criterion_config() -> Criterion {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
        .expect("single-thread comparison pool should initialize");
    Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(4))
        .sample_size(30)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_approach_comparison
);
criterion_main!(benches);
