use byteseek::{CaseMode, Match, SearchConfig, SearchError, Searcher};

fn build_searcher(patterns: &[&str], config: SearchConfig) -> Searcher {
    Searcher::new(patterns.iter().copied(), config).expect("valid approximate test patterns")
}

#[test]
fn approximate_single_pattern_matches_common_typos() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR"], config);
    let haystack = "EROR ERROR ERRPR";

    let matches = searcher.find(haystack);
    let spans = matches
        .iter()
        .map(|matched| (matched.start, matched.end, matched.pattern_index))
        .collect::<Vec<_>>();

    assert_eq!(spans, vec![(0, 4, 0), (5, 10, 0), (11, 16, 0)]);
    assert_eq!(
        searcher.find_first(haystack),
        Some(*matches.first().expect("at least one approximate match"))
    );
}

#[test]
fn approximate_search_respects_ascii_case_and_whole_word() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .whole_word(true)
        .max_edit_distance(Some(1))
        .build();
    let searcher = build_searcher(&["warn"], config);

    let matches = searcher.find("WARN worn warning swarn");
    let offsets = matches
        .iter()
        .map(|matched| (matched.start, matched.end))
        .collect::<Vec<_>>();

    assert_eq!(offsets, vec![(0, 4), (5, 9)]);
}

#[test]
fn approximate_search_supports_overlapping_matches() {
    let config = SearchConfig::builder()
        .overlapping(true)
        .max_edit_distance(Some(1))
        .build();
    let searcher = build_searcher(&["anna"], config);

    let matches = searcher.find("bananana");
    let starts = matches
        .iter()
        .map(|matched| matched.start)
        .collect::<Vec<_>>();

    assert_eq!(starts, vec![1, 3, 5]);
}

#[test]
fn approximate_line_search_does_not_cross_newlines() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR"], config);
    let haystack = "ER\nROR\nEROR\nERROR\n";

    let lines = searcher.find_lines(haystack);
    let matched_lines = lines.iter().map(|line| line.line).collect::<Vec<_>>();

    assert_eq!(matched_lines, vec!["EROR", "ERROR"]);
    assert_eq!(searcher.count_matching_lines(haystack), 2);
}

#[test]
fn approximate_multi_pattern_preserves_pattern_indexes() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR", "WARN"], config);
    let haystack = "EROR WARM";

    let matches = searcher.find(haystack);
    let projections = matches
        .iter()
        .map(|matched| {
            (
                matched.start,
                matched.end,
                matched.pattern_index,
                &haystack[matched.start..matched.end],
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(projections, vec![(0, 4, 0, "EROR"), (5, 9, 1, "WARM")]);
}

#[test]
fn approximate_max_matches_limit_is_honored() {
    let config = SearchConfig::builder()
        .max_matches(Some(1))
        .max_edit_distance(Some(1))
        .build();
    let searcher = build_searcher(&["ERROR"], config);

    assert_eq!(searcher.find("EROR ERROR").len(), 1);
    assert_eq!(searcher.count("EROR ERROR"), 1);
    assert!(searcher.contains("EROR ERROR"));
}

#[test]
fn approximate_matching_handles_boundary_typos_without_crossing_word_breaks() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR"], config);
    let haystack = "XERROR ERRORX ERRO RERROR";

    let matches = searcher.find(haystack);
    let spans = matches
        .iter()
        .map(|matched| {
            (
                matched.start,
                matched.end,
                &haystack[matched.start..matched.end],
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        spans,
        vec![
            (1, 6, "ERROR"),
            (7, 12, "ERROR"),
            (14, 20, "ERRO R"),
            (20, 25, "ERROR"),
        ]
    );
}

#[test]
fn approximate_matching_uses_deterministic_tie_breaking_for_same_region() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR", "ERROP"], config);

    let matches = searcher.find("ERORP");
    let projection = matches
        .iter()
        .map(|matched| (matched.start, matched.end, matched.pattern_index))
        .collect::<Vec<_>>();

    assert_eq!(projection, vec![(0, 4, 0)]);
}

#[test]
fn approximate_matching_respects_zero_limit_and_empty_haystack() {
    let zero_limit = SearchConfig::builder()
        .max_matches(Some(0))
        .max_edit_distance(Some(1))
        .build();
    let zero_limit_searcher = build_searcher(&["ERROR"], zero_limit);
    assert!(zero_limit_searcher.find("EROR").is_empty());

    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR"], config);
    assert!(searcher.find("").is_empty());
    assert_eq!(searcher.find_first(""), None);
    assert!(!searcher.contains(""));
}

#[test]
fn approximate_matching_handles_crlf_line_boundaries() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = build_searcher(&["ERROR"], config);
    let haystack = "EROR\r\nOK\r\nERRPR\r\n";

    let lines = searcher.find_lines(haystack);
    let matched_lines = lines.iter().map(|line| line.line).collect::<Vec<_>>();

    assert_eq!(matched_lines, vec!["EROR", "ERRPR"]);
    assert_eq!(searcher.count_matching_lines(haystack), 2);
}

#[test]
fn too_large_edit_distance_is_rejected() {
    let config = SearchConfig::builder()
        .max_edit_distance(Some(usize::MAX))
        .build();
    let error = match Searcher::new(["abc"], config) {
        Ok(_) => panic!("oversized edit distance should fail"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        SearchError::EditDistanceTooLarge {
            distance: usize::MAX,
        }
    );
}

/// Deterministic xorshift generator for reproducible pseudo-random corpora.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Random text dense in near-misses of short patterns, mixing LF, CRLF and bare CR endings.
fn near_miss_corpus(seed: u64, len: usize) -> String {
    const ALPHABET: &[u8] = b"erorERO wn_\n\n\r";
    let mut state = seed;
    let mut text = (0..len)
        .map(|_| char::from(ALPHABET[(next_random(&mut state) % ALPHABET.len() as u64) as usize]))
        .collect::<String>();
    if seed.is_multiple_of(2) {
        text.push('\r');
    }
    text
}

/// Reference semantics: each `str::lines` line is searched independently.
fn per_line_reference(searcher: &Searcher, haystack: &str) -> Vec<(usize, String, Vec<Match>)> {
    haystack
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let matches = searcher.find(line);
            (!matches.is_empty()).then(|| (index + 1, line.to_owned(), matches))
        })
        .collect()
}

#[test]
fn approximate_find_lines_equals_independent_per_line_search() {
    let configs = [
        SearchConfig::builder().max_edit_distance(Some(1)).build(),
        SearchConfig::builder()
            .max_edit_distance(Some(1))
            .case_mode(CaseMode::AsciiInsensitive)
            .build(),
        SearchConfig::builder()
            .max_edit_distance(Some(1))
            .whole_word(true)
            .build(),
        SearchConfig::builder()
            .max_edit_distance(Some(1))
            .overlapping(true)
            .build(),
    ];
    let pattern_sets: [&[&str]; 3] = [&["eror"], &["eror", "wnor", "rore"], &["ERROR", "eo_w"]];

    for (seed, len) in [
        (1_u64, 64_usize),
        (2, 900),
        (3, 4_096),
        (4, 40_000),
        (6, 700_000),
    ] {
        let haystack = near_miss_corpus(seed, len);
        for config in configs {
            for patterns in pattern_sets {
                let searcher = build_searcher(patterns, config);
                let expected = per_line_reference(&searcher, &haystack);
                for actual in [
                    searcher.find_lines(&haystack),
                    searcher.find_lines_parallel(&haystack),
                ] {
                    let actual = actual
                        .into_iter()
                        .map(|line| {
                            (
                                line.line_number,
                                line.line.to_owned(),
                                line.matches.to_vec(),
                            )
                        })
                        .collect::<Vec<_>>();
                    assert!(
                        actual == expected,
                        "seed {seed}, len {len}, patterns {patterns:?}, config {config:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn overlapping_approximate_matches_do_not_depend_on_thread_count() {
    // Large enough to split across threads, with an "anna" at every possible split point.
    let haystack = format!("{}x", "xanna".repeat(64_003));
    let config = SearchConfig::builder()
        .overlapping(true)
        .max_edit_distance(Some(1))
        .build();
    let searcher = build_searcher(&["anna"], config);
    let run = |threads: usize| {
        in_pool(threads, || {
            (searcher.find(&haystack), searcher.count(&haystack))
        })
    };

    let expected = run(1);

    assert_eq!(expected.0.len(), expected.1);
    for threads in [2, 3, 4, 7, 16] {
        assert_eq!(run(threads), expected, "{threads} threads");
    }
}

#[test]
fn approximate_line_counts_are_the_lines_found_whatever_the_thread_count() {
    // "ERO\nR" is within one edit of the pattern, and so is "ROR" in the second line.
    let haystack = "ERO\nROR\n".repeat(70_000);
    let searcher = build_searcher(
        &["EROR"],
        SearchConfig::builder().max_edit_distance(Some(1)).build(),
    );
    let run = |threads: usize| in_pool(threads, || searcher.count_matching_lines(&haystack));

    let expected = searcher.find_lines(&haystack).len();

    assert_eq!(searcher.count_matching_lines("ERO\nROR\n"), 2);
    assert_eq!(expected, 140_000);
    for threads in [1, 2, 3, 4, 7, 16] {
        assert_eq!(run(threads), expected, "{threads} threads");
    }
}

/// Runs `search` in a rayon pool of `threads` threads.
fn in_pool<T: Send>(threads: usize, search: impl FnOnce() -> T + Send) -> T {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("a thread pool starts")
        .install(search)
}

/// Lines mixing near misses of the test patterns with filler, using varied line endings and a
/// few lines long enough to span several regions. About one word in `words_per_near_miss` is a
/// near miss.
fn text_of_near_misses(words_per_near_miss: u64) -> String {
    let words = [
        "ERROR",
        "EROR",
        "ERRO",
        "ROR",
        "error",
        "sttus=500",
        "status=500",
        "status=200",
        "WARN",
        "worn",
        "x",
        "parse_record",
        "ERORR",
        "E",
        "EROR.EROR",
        // `aaabcd` with one substitution, at offset 2.
        "aaaaabXd",
        "aaaa",
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = |bound: u64| (next_random(&mut state) % bound) as usize;
    let mut text = String::new();
    for line in 0..40_000 {
        let word_count = if line % 9_000 == 4_500 {
            30_000
        } else {
            next(6)
        };
        for _ in 0..word_count {
            let word = if next(words_per_near_miss) == 0 {
                words[next(words.len() as u64)]
            } else {
                "lilac"
            };
            text.push_str(word);
            text.push_str(["", " ", "="][next(3)]);
        }
        text.push_str(["\n", "\r\n", "\n\n", "\r \n"][next(4)]);
    }
    text.push_str("EROR at the end without a line feed\r");
    text
}

#[test]
fn approximate_line_counts_are_the_lines_with_a_match_in_dense_and_sparse_texts() {
    let one_edit = SearchConfig::builder().max_edit_distance(Some(1));
    let searchers = [
        build_searcher(&["EROR"], one_edit.build()),
        build_searcher(&["EROR", "sttus=500"], one_edit.build()),
        build_searcher(&["EROR", "worn", "parse_rekord"], one_edit.build()),
        build_searcher(
            &["eror", "WORN"],
            one_edit.case_mode(CaseMode::AsciiInsensitive).build(),
        ),
        build_searcher(&["EROR", "worn"], one_edit.whole_word(true).build()),
        build_searcher(&["EROR"], one_edit.overlapping(true).build()),
        // A half that overlaps itself where it repeats.
        build_searcher(&["aaabcd"], one_edit.build()),
        // Counted by searching: halves one pass can't tell apart, and five patterns.
        build_searcher(&["EROR", "ERORS"], one_edit.build()),
        build_searcher(
            &["EROR", "worn", "sttus=500", "parse_rekord", "WORN"],
            one_edit.build(),
        ),
    ];

    // Dense (forcing the count to fall back to searching) and sparse near misses.
    for words_per_near_miss in [1, 40] {
        let text = text_of_near_misses(words_per_near_miss);
        for (index, searcher) in searchers.iter().enumerate() {
            let expected = text.lines().filter(|line| searcher.contains(line)).count();
            assert!(expected > 50, "searcher {index}, {words_per_near_miss}");
            assert_eq!(
                searcher.find_lines(&text).len(),
                expected,
                "searcher {index}, {words_per_near_miss}"
            );
            for threads in [1, 2, 5, 12] {
                let count = in_pool(threads, || searcher.count_matching_lines(&text));

                assert_eq!(
                    count, expected,
                    "searcher {index}, {words_per_near_miss}, {threads} threads"
                );
            }
        }
    }
}

#[test]
fn approximate_search_finds_windows_around_a_half_that_overlaps_itself() {
    let searcher = build_searcher(
        &["aaabcd"],
        SearchConfig::builder().max_edit_distance(Some(1)).build(),
    );
    // `aaa` occurs at 0, 1 and 2; only the last one starts a window within one edit.
    let line = "aaaaabXd";
    let text = format!("{line}\n").repeat(3);

    let found = searcher.find(line);
    let contained = searcher.contains(line);
    let lines_found = searcher.find_lines(&text).len();
    let lines_counted = searcher.count_matching_lines(&text);

    assert_eq!(
        found,
        vec![Match {
            start: 2,
            end: 8,
            pattern_index: 0
        }]
    );
    assert!(contained);
    assert_eq!((lines_found, lines_counted), (3, 3));
}
