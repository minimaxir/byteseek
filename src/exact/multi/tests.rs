use super::anchors::{ANCHOR_MIN_HAYSTACK_LEN, ANCHOR_SAMPLE_LEN, AnchoredSearcher};
use super::scan::packed_eligible;
use super::{
    CaseMode, FOLDED_CHUNK_LEN, Match, MultiPatternEngine, SearchConfig, WORD_BLOCK_LEN,
    build_automaton, candidate_match, line_around, matching_line_bytes,
};

/// Builds log lines where every line holds the prefixes of `status=500` and
/// `request_id=1024`, but few lines hold the whole patterns.
fn status_log(lines: usize) -> String {
    (0..lines)
        .map(|index| {
            let level = ["INFO", "WARN", "Error", "ERROR"][index % 4];
            let status = if index % 17 == 0 { 500 } else { 200 };
            format!("{level} request_id={index} status={status} done\n")
        })
        .collect()
}

fn reference_line_count(patterns: &[Vec<u8>], haystack: &[u8], case_mode: CaseMode) -> usize {
    haystack
        .split(|&byte| byte == b'\n')
        .filter(|line| {
            patterns.iter().any(|pattern| {
                line.windows(pattern.len()).any(|window| match case_mode {
                    CaseMode::Sensitive => window == pattern,
                    CaseMode::AsciiInsensitive => window.eq_ignore_ascii_case(pattern),
                })
            })
        })
        .count()
}

#[cfg(target_arch = "aarch64")]
#[test]
fn packed_searcher_preserves_leftmost_longest_and_pattern_index_ties() {
    // Imported here, so targets that leave this test out import nothing unused.
    use super::build_packed_searcher;

    let patterns = vec![b"a".to_vec(), b"ab".to_vec(), b"ab".to_vec()];
    let searcher = build_packed_searcher(&patterns, SearchConfig::default())
        .expect("AArch64 targets support the packed NEON searcher");
    let engine = MultiPatternEngine::new(&patterns, SearchConfig::default())
        .expect("eligible packed engine should build");
    // The standard automaton is deferred exactly when a packed searcher exists, which AArch64
    // NEON always provides for these patterns.
    #[cfg(target_arch = "aarch64")]
    assert!(engine.packed_leftmost_longest.is_some());
    assert_eq!(
        engine.standard.get().is_none(),
        engine.packed_leftmost_longest.is_some()
    );
    assert!(engine.line_matcher.is_none());
    assert!(engine.leftmost_longest.is_none());
    assert_eq!(engine.standard().patterns_len(), patterns.len());

    let matches = searcher
        .find_iter(&b"ab ab".as_slice())
        .map(candidate_match)
        .collect::<Vec<_>>();

    assert_eq!(
        matches
            .iter()
            .map(|matched| (matched.start, matched.end, matched.pattern_index))
            .collect::<Vec<_>>(),
        vec![(0, 2, 1), (3, 5, 1)]
    );
}

#[test]
fn packed_searcher_eligibility_bounds_patterns_bytes_and_matching_modes() {
    let default_config = SearchConfig::default();
    assert!(packed_eligible(&vec![b"a".to_vec(); 128], default_config));
    assert!(!packed_eligible(&vec![b"a".to_vec(); 129], default_config));
    assert!(packed_eligible(
        &[vec![b'a'; 2_048], vec![b'b'; 2_048]],
        default_config
    ));
    assert!(!packed_eligible(
        &[vec![b'a'; 2_049], vec![b'b'; 2_048]],
        default_config
    ));
    assert!(!packed_eligible(
        &[b"a".to_vec(), b"b".to_vec()],
        SearchConfig::builder().overlapping(true).build()
    ));
    // Whole-word search counts lines with it, checking words around each hit.
    assert!(packed_eligible(
        &[b"a".to_vec(), b"b".to_vec()],
        SearchConfig::builder().whole_word(true).build()
    ));
}

fn insensitive_config() -> SearchConfig {
    SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build()
}

/// Asserts every non-overlapping engine API agrees with a leftmost-longest automaton.
fn assert_matches_automaton(patterns: &[Vec<u8>], haystack: &[u8], config: SearchConfig) {
    let pattern_lengths = patterns.iter().map(Vec::len).collect::<Vec<_>>();
    let engine = MultiPatternEngine::new(patterns, config).expect("valid patterns");
    let baseline = build_automaton(patterns, config, aho_corasick::MatchKind::LeftmostLongest)
        .expect("baseline automaton should compile");
    let expected = baseline
        .find_iter(haystack)
        .map(candidate_match)
        .collect::<Vec<_>>();
    let expected_lines = reference_line_count(patterns, haystack, config.case_mode());
    let mut visited = Vec::<Match>::new();
    let planned = engine.block_plan(haystack, config);
    engine.visit_planned(haystack, planned, |matched| {
        visited.push(matched);
        true
    });

    for limit in [
        0,
        1,
        3,
        5,
        expected.len().saturating_sub(1),
        expected.len(),
        usize::MAX,
    ] {
        let limited_len = expected.len().min(limit);
        assert_eq!(
            engine.search(haystack, config, &pattern_lengths, limit),
            expected[..limited_len]
        );
        assert_eq!(
            engine.count_planned(haystack, config, limit, planned),
            Some(limited_len)
        );
    }
    assert_eq!(visited, expected);
    assert_eq!(
        engine.find_first(haystack, config, &pattern_lengths),
        expected.first().copied()
    );
    assert_eq!(
        engine.contains(haystack, config, &pattern_lengths),
        !expected.is_empty()
    );

    assert_eq!(
        engine.count_matching_lines(haystack, config, &pattern_lengths),
        expected_lines
    );
}

#[test]
fn block_scanner_serves_long_haystacks_like_the_automaton() {
    let patterns = vec![b"status".to_vec(), b"status=500".to_vec(), b"err".to_vec()];
    // A match every few bytes, in mixed case, well past the scanner's minimum length.
    let dense = "Status=500 err STATUS=200 ERR status=500\n".repeat(1_200);
    let sparse_body = format!(
        "{}{}",
        &dense[..4_096],
        "no match in this line\n".repeat(3_000)
    );

    for config in [SearchConfig::default(), insensitive_config()] {
        let engine = MultiPatternEngine::new(&patterns, config).expect("valid patterns");

        assert!(engine.block_plan(dense.as_bytes(), config).is_some());
        assert!(engine.block_plan(sparse_body.as_bytes(), config).is_some());
        assert!(
            engine
                .block_plan(&dense.as_bytes()[..16 * 1024], config)
                .is_none()
        );
        let overlapping = SearchConfig::builder()
            .case_mode(config.case_mode())
            .overlapping(true)
            .build();
        assert!(engine.block_plan(dense.as_bytes(), overlapping).is_none());
        assert_matches_automaton(&patterns, dense.as_bytes(), config);
        assert_matches_automaton(&patterns, sparse_body.as_bytes(), config);
    }
}

#[test]
fn packed_search_keeps_sparse_matches_of_many_probes() {
    // Six patterns over four letters need four probes each, and their matches are sparse.
    let patterns = [
        "ACGTACGT", "GTACGGTA", "TTTTTTTT", "CCCCCCCC", "GGGGGGGG", "AAAAAAAA",
    ]
    .map(|pattern| pattern.as_bytes().to_vec());
    let mut seed = 0x1234_5678_9abc_def0_u64;
    let dna = (0..80_000)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            b"ACGT"[(seed >> 62) as usize]
        })
        .collect::<Vec<u8>>();
    let config = SearchConfig::default();
    let engine = MultiPatternEngine::new(&patterns, config).expect("valid patterns");

    assert!(engine.block_plan(&dna, config).is_none());
    assert_matches_automaton(&patterns, &dna, config);
}

#[test]
fn folded_packed_search_matches_leftmost_longest_automaton_across_chunks() {
    let patterns = vec![
        b"ab".to_vec(),
        b"AB".to_vec(),
        b"abc".to_vec(),
        b"status=500".to_vec(),
    ];
    let mut haystack = Vec::new();
    let mut index = 0_usize;
    while haystack.len() < 3 * FOLDED_CHUNK_LEN {
        let line: &[u8] = match index % 5 {
            0 => b"ABC ab STATUS=500 Abc\r\n",
            1 => b"nothing to see here\n",
            2 => b"x aB status=50 abC\n",
            3 => b"",
            _ => b"plain\n",
        };
        haystack.extend_from_slice(line);
        if index % 5 == 3 {
            haystack.push(b'\n');
        }
        index += 1;
    }

    assert!(
        MultiPatternEngine::new(&patterns, insensitive_config())
            .expect("valid patterns")
            .fold_haystack
    );
    assert_matches_automaton(&patterns, &haystack, insensitive_config());
    assert_matches_automaton(&patterns, b"", insensitive_config());
    assert_matches_automaton(&patterns, b"STATUS=500", insensitive_config());
}

#[test]
fn folded_packed_search_keeps_matches_on_lines_longer_than_a_chunk() {
    let patterns = vec![b"needle".to_vec(), b"hay".to_vec()];
    let mut haystack = vec![b'x'; 2 * FOLDED_CHUNK_LEN + 17];
    let middle = FOLDED_CHUNK_LEN - 3;
    haystack[middle..middle + 6].copy_from_slice(b"NeEdLe");
    haystack.extend_from_slice(b"\nHAY\n");

    assert_matches_automaton(&patterns, &haystack, insensitive_config());
}

#[test]
fn native_count_and_collection_limits_agree_for_packed_and_aho_backends() {
    let mut patterns = vec![b"needle".to_vec(), b"warning".to_vec(), b"info".to_vec()];
    let haystack = "needle warning info\nNEEDLE WARNING INFO\n".repeat(3_000);
    for pattern_count in [3, 129] {
        patterns.extend(
            (patterns.len()..pattern_count).map(|index| format!("absent{index}").into_bytes()),
        );
        for config in [SearchConfig::default(), insensitive_config()] {
            let engine = MultiPatternEngine::new(&patterns, config).expect("valid patterns");
            if pattern_count == 129 {
                assert!(engine.packed_leftmost_longest.is_none());
            } else if cfg!(target_arch = "aarch64") {
                assert!(engine.packed_leftmost_longest.is_some());
            }
            assert_matches_automaton(&patterns, haystack.as_bytes(), config);
            assert_matches_automaton(&patterns, b"", config);
            assert_matches_automaton(&patterns, b"no hits", config);
        }
    }
}

#[test]
fn packed_eligibility_rejects_insensitive_newline_patterns_only() {
    let config = insensitive_config();

    assert!(packed_eligible(
        &[b"warn".to_vec(), b"error".to_vec()],
        config
    ));
    assert!(!packed_eligible(
        &[b"warn".to_vec(), b"a\nb".to_vec()],
        config
    ));
    assert!(packed_eligible(
        &[b"warn".to_vec(), b"a\nb".to_vec()],
        SearchConfig::default()
    ));
}

#[test]
fn anchored_searcher_skips_common_prefixes() {
    let patterns = vec![b"status=500".to_vec(), b"request_id=1024".to_vec()];
    let haystack = status_log(400);

    let anchored =
        AnchoredSearcher::new(&patterns, &haystack.as_bytes()[..ANCHOR_SAMPLE_LEN], false)
            .expect("every line holds both prefixes");

    assert!(
        anchored
            .holders
            .iter()
            .flatten()
            .all(|&(_, offset)| offset > 0)
    );
}

#[test]
fn anchored_searcher_declines_rare_prefixes() {
    let patterns = vec![b"ERROR".to_vec(), b"WARN".to_vec()];
    let haystack = status_log(400);

    let anchored =
        AnchoredSearcher::new(&patterns, &haystack.as_bytes()[..ANCHOR_SAMPLE_LEN], false);

    assert!(anchored.is_none());
}

#[test]
fn anchored_line_counts_match_per_line_matching() {
    let haystack = status_log(ANCHOR_MIN_HAYSTACK_LEN / 30);
    let pattern_sets: [&[&str]; 3] = [
        &["ERROR", "WARN", "status=500", "request_id=1024"],
        &["status=500", "tatus=500"],
        &["error", "status=500", "id=77"],
    ];

    for patterns in pattern_sets {
        for case_mode in [CaseMode::Sensitive, CaseMode::AsciiInsensitive] {
            let config = SearchConfig::builder().case_mode(case_mode).build();
            let pattern_bytes = patterns
                .iter()
                .map(|pattern| pattern.as_bytes().to_vec())
                .collect::<Vec<_>>();
            let engine = MultiPatternEngine::new(&pattern_bytes, config)
                .expect("literal patterns should compile");
            let pattern_lengths = pattern_bytes.iter().map(Vec::len).collect::<Vec<_>>();

            let count = engine.count_matching_lines(haystack.as_bytes(), config, &pattern_lengths);

            assert!(
                engine
                    .line_count_plan(haystack.as_bytes(), config)
                    .0
                    .is_some()
            );
            assert_eq!(
                count,
                reference_line_count(&pattern_bytes, haystack.as_bytes(), case_mode)
            );
        }
    }
}

#[test]
fn lazy_standard_automaton_from_folded_patterns_matches_an_eager_build() {
    let config = insensitive_config();
    let patterns = vec![b"WaRn".to_vec(), b"warn".to_vec(), b"ErRoR".to_vec()];
    let haystack = b"warn WARN Error eRRor wArNing";
    let engine = MultiPatternEngine::new(&patterns, config).expect("patterns should compile");
    assert!(engine.packed_leftmost_longest.is_some());
    assert!(engine.standard.get().is_none());
    let eager = build_automaton(&patterns, config, aho_corasick::MatchKind::Standard)
        .expect("patterns should build an automaton");
    let spans = |automaton: &aho_corasick::AhoCorasick| {
        automaton
            .find_overlapping_iter(haystack)
            .map(|matched| (matched.start(), matched.end(), matched.pattern().as_usize()))
            .collect::<Vec<_>>()
    };

    let lazy = spans(engine.standard());

    assert!(!lazy.is_empty());
    assert_eq!(lazy, spans(&eager));
}

#[test]
fn whole_word_line_counts_are_the_lines_of_whole_word_matches() {
    let patterns = [
        b"foo b".to_vec(),
        b"foo".to_vec(),
        b"bar".to_vec(),
        b"oob".to_vec(),
    ];
    let pattern_lengths = patterns.iter().map(Vec::len).collect::<Vec<_>>();
    // The longest pattern at a start can fail where a shorter one is a word, and occurrences
    // overlap, sit beside word bytes or invalid UTF-8, or differ in case.
    let kinds: [&[u8]; 9] = [
        b"foo bx\n",
        b"xfoo bar\n",
        b"foobar\n",
        b"oob foo\n",
        b"FOO B\n",
        b"a foo_b foo_\n",
        b"\xc3foo\xa9\n",
        b"nothing here\n",
        b"\n",
    ];
    // Several folded chunks, so words also meet chunk edges, and sections dense and sparse
    // in matching lines, each over a few blocks, so search switches between its strategies.
    let section_lines = 3 * WORD_BLOCK_LEN / 8;
    let mut haystack = Vec::new();
    for index in 0..(4 * section_lines) {
        let kind = if (index / section_lines) % 2 == 1 && index % 50 != 0 {
            b"nothing here\n"
        } else {
            kinds[(index * 5 + index / 7) % kinds.len()]
        };
        haystack.extend_from_slice(kind);
    }
    assert!(haystack.len() > 4 * FOLDED_CHUNK_LEN);
    haystack.extend_from_slice(b"last foo");
    for case_mode in [CaseMode::Sensitive, CaseMode::AsciiInsensitive] {
        let config = SearchConfig::builder()
            .case_mode(case_mode)
            .whole_word(true)
            .build();
        let engine = MultiPatternEngine::new(&patterns, config).expect("engine should build");
        assert!(engine.packed_leftmost_longest.is_some());

        // A finite limit searches the whole haystack instead of only its matching lines.
        let whole = engine.search(&haystack, config, &pattern_lengths, haystack.len());
        let by_lines = engine.search(&haystack, config, &pattern_lengths, usize::MAX);
        assert_eq!(by_lines, whole, "{case_mode:?}");
        let mut counter = crate::LineCounter::new(&haystack);
        for matched in whole {
            counter.record(matched.start);
        }
        let naive = haystack
            .split(|&byte| byte == b'\n')
            .filter(|line| {
                patterns.iter().any(|pattern| {
                    line.windows(pattern.len())
                        .enumerate()
                        .any(|(start, window)| {
                            let equal = match case_mode {
                                CaseMode::Sensitive => window == pattern.as_slice(),
                                CaseMode::AsciiInsensitive => window.eq_ignore_ascii_case(pattern),
                            };
                            equal && crate::is_word_boundary(line, start, start + pattern.len())
                        })
                })
            })
            .count();

        let count = engine.count_matching_lines(&haystack, config, &pattern_lengths);
        assert_eq!(count, counter.count(), "{case_mode:?}");
        assert_eq!(count, naive, "{case_mode:?}");
    }
}

#[test]
fn line_around_spans_the_line_content_without_its_line_feed() {
    let haystack = b"ab\ncde\n\nf";

    assert_eq!(line_around(haystack, 0), 0..2);
    assert_eq!(line_around(haystack, 4), 3..6);
    assert_eq!(line_around(haystack, 7), 7..7);
    assert_eq!(line_around(haystack, 8), 8..9);
}

#[test]
fn matching_line_bytes_counts_each_line_once() {
    let haystack = b"aa aa\nbbb\ncc\n";
    let found = |start, end| Match {
        pattern_index: 0,
        start,
        end,
    };

    let matches = [found(0, 2), found(3, 5), found(10, 12)];

    assert_eq!(matching_line_bytes(haystack, &matches), 5 + 2);
    assert_eq!(matching_line_bytes(haystack, &[]), 0);
}
