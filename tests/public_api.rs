use byteseek::{
    CaseMode, Match, PathOrder, SearchConfig, SearchConfigBuilder, SearchError, Searcher,
    StructuredMode, StructuredPredicateHandling,
};

const SHERLOCK: &str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";

const SHERLOCK_CRLF: &str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock\r
Holmeses, success in the province of detective work must always\r
be, to a very large extent, the result of luck. Sherlock Holmes\r
can extract a clew from a wisp of straw or a flake of cigar ash;\r
but Doctor Watson has to have it taken out for him and dusted,\r
and exhibited clearly, with a label attached.\r
";

fn build_searcher(patterns: &[&str], config: SearchConfig) -> Searcher {
    Searcher::new(patterns.iter().copied(), config).expect("valid test patterns")
}

#[test]
fn builder_defaults_match_config_default() {
    let config = SearchConfig::builder().build();

    assert_eq!(config, SearchConfig::default());
    assert_eq!(config, SearchConfigBuilder::default().build());
    assert_eq!(config, SearchConfigBuilder::new().build());
    assert_eq!(
        config.structured_predicate_handling(),
        StructuredPredicateHandling::Ignore
    );
    assert_eq!(config.case_mode(), CaseMode::Sensitive);
    assert!(!config.boolean_queries());
    assert!(!config.whole_word());
    assert!(!config.overlapping());
    assert!(!config.ignore_gitignore());
    assert_eq!(config.path_order(), PathOrder::Discovery);
    assert_eq!(config.max_matches(), None);
    assert_eq!(config.max_edit_distance(), None);
    assert_eq!(config.structured_mode(), None);
}

#[test]
fn builder_customizes_all_fields() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .boolean_queries(true)
        .whole_word(true)
        .overlapping(true)
        .ignore_gitignore(true)
        .path_order(PathOrder::Path)
        .max_matches(Some(4))
        .max_edit_distance(Some(1))
        .structured_mode(Some(StructuredMode::Json))
        .structured_predicate_handling(StructuredPredicateHandling::Error)
        .build();

    assert_eq!(config.case_mode(), CaseMode::AsciiInsensitive);
    assert!(config.boolean_queries());
    assert!(config.whole_word());
    assert!(config.overlapping());
    assert!(config.ignore_gitignore());
    assert_eq!(config.path_order(), PathOrder::Path);
    assert_eq!(config.max_matches(), Some(4));
    assert_eq!(config.max_edit_distance(), Some(1));
    assert_eq!(config.structured_mode(), Some(StructuredMode::Json));
    assert_eq!(
        config.structured_predicate_handling(),
        StructuredPredicateHandling::Error
    );
}

#[test]
fn searcher_rejects_invalid_patterns() {
    let empty_set = match Searcher::new(Vec::<String>::new(), SearchConfig::default()) {
        Ok(_) => panic!("empty set should fail"),
        Err(error) => error,
    };
    assert_eq!(empty_set, SearchError::EmptyPatternSet);

    let empty_pattern = match Searcher::new(["abc", ""], SearchConfig::default()) {
        Ok(_) => panic!("empty pattern should fail"),
        Err(error) => error,
    };
    assert_eq!(empty_pattern, SearchError::EmptyPattern { index: 1 });
}

#[test]
fn structured_mode_rejects_approximate_matching() {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .max_edit_distance(Some(1))
        .build();
    let error = match Searcher::new(["$.user.name"], config) {
        Ok(_) => panic!("structured approximate search should fail"),
        Err(error) => error,
    };

    assert_eq!(error, SearchError::StructuredApproximateUnsupported);
}

#[test]
fn boolean_query_mode_rejects_approximate_matching() {
    let config = SearchConfig::builder()
        .boolean_queries(true)
        .max_edit_distance(Some(1))
        .build();
    let error = match Searcher::new(["time AND date"], config) {
        Ok(_) => panic!("boolean approximate search should fail"),
        Err(error) => error,
    };

    assert_eq!(error, SearchError::BooleanApproximateUnsupported);
}

#[test]
fn single_pattern_case_sensitive_search() {
    let searcher = build_searcher(&["cat"], SearchConfig::default());

    let matches = searcher.find("cat scatter catalog");
    assert_eq!(matches.len(), 3);
    assert_eq!(
        matches,
        vec![
            Match {
                start: 0,
                end: 3,
                pattern_index: 0,
            },
            Match {
                start: 5,
                end: 8,
                pattern_index: 0,
            },
            Match {
                start: 12,
                end: 15,
                pattern_index: 0,
            },
        ]
    );
}

#[test]
fn single_pattern_ascii_case_insensitive_search() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["error"], config);

    let matches = searcher.find("ERROR Error error eRrOr");
    assert_eq!(matches.len(), 4);
    assert!(searcher.contains("No ERRORS"));
}

#[test]
fn adaptive_packed_single_pattern_preserves_parallel_boundaries_and_limits() {
    let pattern = "ACGTAC";
    let mut bytes = vec![b'A'; 600_000];
    let chunk_count = rayon::current_num_threads()
        .min(bytes.len() / (16 * 1024))
        .max(2);
    let chunk_size = bytes.len() / chunk_count;
    let mut starts = vec![10, bytes.len() - pattern.len()];
    starts.extend((1..chunk_count).map(|index| index * chunk_size - 2));
    starts.sort_unstable();
    starts.dedup();
    for start in starts {
        bytes[start..start + pattern.len()].copy_from_slice(pattern.as_bytes());
    }
    let text = String::from_utf8(bytes).expect("test corpus is ASCII");
    let expected = text
        .match_indices(pattern)
        .map(|(start, matched)| Match {
            start,
            end: start + matched.len(),
            pattern_index: 0,
        })
        .collect::<Vec<_>>();

    let searcher = build_searcher(&[pattern], SearchConfig::default());
    assert_eq!(searcher.find(&text), expected);

    let limited = build_searcher(
        &[pattern],
        SearchConfig::builder().max_matches(Some(3)).build(),
    );
    assert_eq!(limited.find(&text), expected[..3]);
}

#[test]
fn overlapping_mode_reports_overlapping_hits() {
    let config = SearchConfig::builder().overlapping(true).build();
    let searcher = build_searcher(&["ana"], config);

    let matches = searcher.find("bananana");
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[0].start, 1);
    assert_eq!(matches[1].start, 3);
    assert_eq!(matches[2].start, 5);
}

#[test]
fn whole_word_mode_filters_embedded_matches() {
    let config = SearchConfig::builder().whole_word(true).build();
    let searcher = build_searcher(&["cat"], config);

    let matches = searcher.find("cat scatter cat catalog");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].start, 0);
    assert_eq!(matches[1].start, 12);
}

#[test]
fn max_matches_is_honored() {
    let config = SearchConfig::builder().max_matches(Some(2)).build();
    let searcher = build_searcher(&["a"], config);

    let matches = searcher.find("aaaa");
    assert_eq!(matches.len(), 2);
    assert_eq!(searcher.count("aaaa"), 2);
    assert_eq!(searcher.find_first("aaaa"), Some(matches[0]));
}

#[test]
fn direct_count_matches_materialized_results() {
    let cases = [
        (
            vec!["needle"],
            SearchConfig::default(),
            "needle hay needle stack needle",
        ),
        (
            vec!["a"],
            SearchConfig::builder()
                .case_mode(CaseMode::AsciiInsensitive)
                .whole_word(true)
                .max_matches(Some(2))
                .build(),
            "A a embedded-a A",
        ),
        (
            vec!["error", "warning", "info"],
            SearchConfig::builder().max_matches(Some(3)).build(),
            "info error warning info error",
        ),
    ];

    for (patterns, config, haystack) in cases {
        let searcher = build_searcher(patterns.as_slice(), config);
        assert_eq!(searcher.count(haystack), searcher.find(haystack).len());
    }
}

#[test]
fn multi_pattern_aho_corasick_finds_all() {
    let config = SearchConfig::builder().overlapping(true).build();
    let searcher = build_searcher(&["he", "she", "his", "hers"], config);

    let matches = searcher.find("ushers and his");
    assert_eq!(searcher.pattern_count(), 4);
    assert_eq!(matches.len(), 4);
    assert_eq!(matches[0].pattern_index, 1);
    assert_eq!(matches[1].pattern_index, 0);
    assert_eq!(matches[2].pattern_index, 3);
    assert_eq!(matches[3].pattern_index, 2);
}

#[test]
fn multi_pattern_non_overlapping_prefers_longest_match() {
    let searcher = build_searcher(&["abc", "bc"], SearchConfig::default());

    let matches = searcher.find("abc");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].pattern_index, 0);
}

#[test]
fn packed_multi_pattern_apis_preserve_limits_and_pattern_index_ties() {
    let config = SearchConfig::builder().max_matches(Some(1)).build();
    let searcher = build_searcher(&["a", "ab", "ab"], config);
    let expected = Match {
        start: 0,
        end: 2,
        pattern_index: 1,
    };

    assert_eq!(searcher.find("ab ab"), vec![expected]);
    assert_eq!(searcher.find_first("ab ab"), Some(expected));
    assert_eq!(searcher.count("ab ab"), 1);
}

#[test]
fn multi_pattern_ascii_insensitive_fallback_preserves_mixed_case_matches() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["warn", "error"], config);

    let matches = searcher.find("WARN Error warn ERROR");
    assert_eq!(
        matches
            .iter()
            .map(|matched| matched.pattern_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 0, 1]
    );
    assert_eq!(
        searcher.find_first("ok ERROR"),
        Some(Match {
            start: 3,
            end: 8,
            pattern_index: 1,
        })
    );
}

#[test]
fn multi_pattern_whole_word_keeps_shorter_when_longer_fails_boundary() {
    let config = SearchConfig::builder().whole_word(true).build();
    let searcher = build_searcher(&["a-", "a"], config);

    let matches = searcher.find("a-b");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].pattern_index, 1);
    assert_eq!(matches[0].start, 0);
    assert_eq!(matches[0].end, 1);
}

#[test]
fn line_search_methods_match_each_other() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["error", "panic"], config);

    let text = "ok\nERROR found\nnope\npanic: bad\n";

    let sequential = searcher.find_lines(text);
    let parallel = searcher.find_lines_parallel(text);

    assert_eq!(sequential, parallel);
    assert_eq!(sequential.len(), 2);
    assert_eq!(sequential[0].line_number, 2);
    assert_eq!(sequential[1].line_number, 4);
}

#[test]
fn large_ascii_insensitive_anchor_line_search_preserves_order_and_offsets() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["error", "warn", "status=500"], config);
    let text = "INFO ok\nERROR status=500\nWARN ok\n".repeat(20_000);

    let lines = searcher.find_lines(&text);

    assert_eq!(lines.len(), 40_000);
    assert_eq!(lines[0].line_number, 2);
    assert_eq!(lines[0].matches.len(), 2);
    assert_eq!(lines[0].matches[0].pattern_index, 0);
    assert_eq!(lines[0].matches[1].pattern_index, 2);
    assert_eq!(lines[1].line_number, 3);
    assert_eq!(lines[1].matches[0].pattern_index, 1);
    assert_eq!(lines[39_999].line_number, 60_000);
    assert_eq!(searcher.find_lines_parallel(&text), lines);
}

#[test]
fn large_sensitive_multi_pattern_line_search_preserves_order_and_offsets() {
    let searcher = build_searcher(&["alpha", "beta"], SearchConfig::default());
    let text = "alpha beta\nnone\n".repeat(60_000);

    let lines = searcher.find_lines(&text);

    assert_eq!(lines.len(), 60_000);
    assert_eq!(lines[0].line_number, 1);
    assert_eq!(lines[0].matches.len(), 2);
    assert_eq!(lines[0].matches[0].start, 0);
    assert_eq!(lines[0].matches[1].start, 6);
    assert_eq!(lines[59_999].line_number, 119_999);
    assert_eq!(searcher.find_lines_parallel(&text), lines);
}

#[test]
fn line_search_respects_per_line_max_matches() {
    let config = SearchConfig::builder().max_matches(Some(1)).build();
    let searcher = build_searcher(&["a"], config);

    let matches = searcher.find_lines("aaaa\naaaa\n");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].matches.len(), 1);
    assert_eq!(matches[1].matches.len(), 1);
}

#[test]
fn count_matching_lines_matches_line_search_results() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["error", "warn"], config);
    let text = "ok\nWARN here\nstill ok\nerror there\nwarn again\n";

    assert_eq!(
        searcher.count_matching_lines(text),
        searcher.find_lines(text).len()
    );
}

#[test]
fn count_matching_lines_falls_back_when_max_matches_is_set() {
    let config = SearchConfig::builder().max_matches(Some(1)).build();
    let searcher = build_searcher(&["a"], config);

    assert_eq!(searcher.count_matching_lines("aaaa\nbbbb\naaaa\n"), 2);
}

#[test]
fn line_search_ignores_cross_line_newline_patterns() {
    let searcher = build_searcher(&["\n"], SearchConfig::default());
    let matches = searcher.find_lines("a\nb\n");
    assert!(matches.is_empty());
}

#[test]
fn ascii_insensitive_anchor_path_handles_tail_anchor_without_panic() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["warn"], config);

    let haystack = "x".repeat(5_000) + "W";
    let matches = searcher.find(haystack.as_str());
    assert!(matches.is_empty());
}

#[test]
fn ascii_insensitive_multi_pattern_apis_handle_short_haystacks() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["aab", "xyz"], config);

    assert!(searcher.find("a").is_empty());
    assert_eq!(searcher.find_first("a"), None);
    assert!(!searcher.contains("a"));
    assert_eq!(searcher.count("a"), 0);
    assert!(searcher.find_lines("a").is_empty());
    assert!(searcher.find_lines_parallel("a").is_empty());
}

#[test]
fn single_byte_case_insensitive_supports_alpha_and_non_alpha() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();

    let alpha = build_searcher(&["a"], config);
    assert_eq!(alpha.count("AaA"), 3);

    let punctuation = build_searcher(&["!"], config);
    assert_eq!(punctuation.count("!a!"), 2);
}

#[test]
fn analogous_issue_34_only_matching_reports_line_numbers_and_offsets() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());

    let lines = searcher.find_lines(SHERLOCK);
    assert_eq!(lines.len(), 2);

    assert_eq!(lines[0].line_number, 1);
    assert_eq!(
        lines[0].line,
        SHERLOCK.lines().next().expect("line 1 exists")
    );
    assert_eq!(lines[0].matches.len(), 1);
    assert_eq!(lines[0].matches[0].start, 56);
    assert_eq!(lines[0].matches[0].end, 64);

    assert_eq!(lines[1].line_number, 3);
    assert_eq!(
        lines[1].line,
        SHERLOCK.lines().nth(2).expect("line 3 exists")
    );
    assert_eq!(lines[1].matches.len(), 1);
    assert_eq!(lines[1].matches[0].start, 48);
    assert_eq!(lines[1].matches[0].end, 56);
}

#[test]
fn analogous_issue_89_count_matching_lines_counts_lines_not_matches() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let haystack = "Sherlock Sherlock\nnope\nSherlock\n";

    assert_eq!(searcher.count(haystack), 3);
    assert_eq!(searcher.count_matching_lines(haystack), 2);
}

#[test]
fn analogous_issue_416_crlf_line_handling_is_correct() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());

    let lines = searcher.find_lines(SHERLOCK_CRLF);
    assert_eq!(lines.len(), 2);
    assert_eq!(searcher.count_matching_lines(SHERLOCK_CRLF), 2);
    assert_eq!(lines[0].line_number, 1);
    assert_eq!(lines[1].line_number, 3);
    assert!(!lines[0].line.ends_with('\r'));
    assert!(!lines[1].line.ends_with('\r'));
    assert!(lines[0].line.ends_with("Sherlock"));
    assert!(lines[1].line.ends_with("Sherlock Holmes"));
}

#[test]
fn analogous_issue_506_whole_word_multi_pattern_keeps_word_boundaries() {
    let config = SearchConfig::builder().whole_word(true).build();
    let searcher = build_searcher(&["min", "max"], config);
    let haystack = "min minimum amin\nmax maximum amax";

    let matches = searcher.find(haystack);
    let matched_text = matches
        .iter()
        .map(|matched| &haystack[matched.start..matched.end])
        .collect::<Vec<_>>();

    assert_eq!(matched_text, vec!["min", "max"]);
    assert_eq!(searcher.count_matching_lines(haystack), 2);
}

#[test]
fn analogous_issue_553_ascii_case_insensitive_counts_mixed_case_lines() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["sherlock"], config);

    assert_eq!(searcher.count_matching_lines(SHERLOCK), 2);
    assert_eq!(searcher.find_lines(SHERLOCK).len(), 2);
}

#[test]
fn analogous_issue_1334_repeated_literals_still_match() {
    let patterns = vec!["1.208.0.0/12"; 40];
    let searcher = Searcher::new(patterns.iter().copied(), SearchConfig::default())
        .expect("duplicate literal set should compile");
    let haystack = "1.208.0.0/12\n";

    assert!(searcher.contains(haystack));
    assert_eq!(searcher.count_matching_lines(haystack), 1);
    assert_eq!(searcher.find_lines(haystack).len(), 1);
}
