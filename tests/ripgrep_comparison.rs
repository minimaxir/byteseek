#[path = "../benches/support/ripgrep.rs"]
mod comparison;

use byteseek::{CaseMode, SearchConfig, Searcher};
use comparison::{build_matcher, build_searcher, count_matching_lines, oracle_count};

#[test]
fn line_count_dispatch_agrees_with_one_and_multiple_workers() {
    let corpus =
        "INFO request complete with no failure\r\nERROR WARN status=500 repeated ERROR\n\n"
            .repeat(12_000);
    let patterns = ["error", "warn", "status=500"];
    for case_mode in [CaseMode::Sensitive, CaseMode::AsciiInsensitive] {
        let config = SearchConfig::builder().case_mode(case_mode).build();
        let searcher = Searcher::new(patterns, config).expect("valid patterns");
        let expected = oracle_count(&patterns, config, &corpus);
        for workers in [1, 2] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .expect("test pool should build");
            assert_eq!(
                pool.install(|| searcher.count_matching_lines(&corpus)),
                expected
            );
        }
    }
}

#[test]
fn matching_line_counts_agree_with_an_independent_oracle() {
    let pattern_sets: &[&[&str]] = &[
        &["ERROR"],
        &["warn"],
        &["ERROR", "WARN", "status=500", "request_id=1024"],
        &["error", "warn", "trace", "status=500"],
        &["a", "aa", "aaa"],
        &["needle", "need", "needle"],
        &["missing", "absent"],
        &["é", "Ä"],
    ];
    let corpora = [
        "",
        "\n",
        "\r\n",
        "unmatched",
        "ERROR",
        "ERROR\n",
        "WARN\r\nERROR\r\n",
        "ERROR WARN ERROR\n\ntrace status=500\nrequest_id=1024",
        "aaaaaa\r\naaaaaa\n",
        "needleneed\nneedle\n",
        "é ä Ä\nÉ\n",
        "\0ERROR\0\nWARN\n",
        "not an ERROR\r\n\r\nwarn",
    ];
    let settings = [CaseMode::Sensitive, CaseMode::AsciiInsensitive]
        .into_iter()
        .flat_map(|case_mode| [false, true].map(|whole_word| (case_mode, whole_word)));
    for (case_mode, whole_word) in settings {
        let config = SearchConfig::builder()
            .case_mode(case_mode)
            .whole_word(whole_word)
            .build();
        for &patterns in pattern_sets {
            let rust = Searcher::new(patterns.iter().copied(), config).expect("valid patterns");
            let matcher = build_matcher(patterns, config);
            let mut ripgrep = build_searcher();
            for corpus in corpora {
                let expected = oracle_count(patterns, config, corpus);
                let context = format!("{patterns:?} {case_mode:?} {whole_word} {corpus:?}");
                assert_eq!(
                    rust.count_matching_lines(corpus),
                    expected,
                    "byteseek {context}"
                );
                assert_eq!(
                    count_matching_lines(&mut ripgrep, &matcher, corpus),
                    expected,
                    "ripgrep {context}"
                );
            }
        }
    }
}
