use super::{SearchConfig, SinglePatternEngine, single_match};
use crate::{CaseMode, Match};

/// Bytes enough for a block plan: several times the shortest haystack one is made for.
const PLANNED_LEN: usize = 256 * 1024;

/// The four ways of matching letters and of letting matches overlap.
fn configs() -> [SearchConfig; 4] {
    [
        (CaseMode::Sensitive, false),
        (CaseMode::Sensitive, true),
        (CaseMode::AsciiInsensitive, false),
        (CaseMode::AsciiInsensitive, true),
    ]
    .map(|(case_mode, overlapping)| {
        SearchConfig::builder()
            .case_mode(case_mode)
            .overlapping(overlapping)
            .build()
    })
}

/// The matches of `pattern` by comparing it at every start of `haystack`.
fn reference(pattern: &[u8], haystack: &[u8], config: SearchConfig) -> Vec<Match> {
    let mut matches = Vec::new();
    let mut start = 0;
    while start + pattern.len() <= haystack.len() {
        let window = &haystack[start..start + pattern.len()];
        let occurs = match config.case_mode() {
            CaseMode::Sensitive => window == pattern,
            CaseMode::AsciiInsensitive => window.eq_ignore_ascii_case(pattern),
        };
        if occurs {
            matches.push(single_match(start, start + pattern.len()));
        }
        start += if occurs && !config.overlapping() {
            pattern.len()
        } else {
            1
        };
    }
    matches
}

/// Text with `word` in either case, alone and overlapping itself, between filler.
fn text_holding(word: &str) -> Vec<u8> {
    let upper = word.to_ascii_uppercase();
    let mut text = Vec::with_capacity(PLANNED_LEN + 64);
    for index in 0.. {
        if text.len() >= PLANNED_LEN {
            break;
        }
        let field = match index % 7 {
            0 => format!("{word}{word} "),
            3 => format!("{upper} "),
            5 => format!("{}{word} ", &word[..word.len() - 1]),
            _ => format!("filler {index} "),
        };
        text.extend_from_slice(field.as_bytes());
    }
    text
}

#[test]
fn block_plans_serve_long_haystacks_and_patterns_matched_without_word_boundaries() {
    let engine = SinglePatternEngine::new(b"ana".to_vec());
    let haystack = text_holding("ana");
    let whole_word = SearchConfig::builder().whole_word(true).build();

    let planned = configs().map(|config| engine.block_plan(&haystack, config).is_some());

    assert_eq!(planned, [true; 4]);
    assert!(engine.block_plan(&haystack, whole_word).is_none());
    assert!(engine.block_plan(&haystack[..1024], configs()[0]).is_none());
    assert!(
        SinglePatternEngine::new(b"a".to_vec())
            .block_plan(&haystack, configs()[0])
            .is_none()
    );
}

#[test]
fn planned_searches_collect_count_and_visit_the_matches_found_at_every_start() {
    let engine = SinglePatternEngine::new(b"ana".to_vec());
    let haystack = text_holding("ana");
    for config in configs() {
        let expected = reference(b"ana", &haystack, config);
        let plan = || engine.block_plan(&haystack, config);
        assert!(plan().is_some(), "{config:?}");

        let collected = engine.search_planned(&haystack, config, usize::MAX, plan());
        let limited = engine.search_planned(&haystack, config, 5, plan());
        let counted = engine.count_planned(&haystack, config, usize::MAX, plan());
        let counted_to_limit = engine.count_planned(&haystack, config, 5, plan());
        let mut visited = Vec::new();
        engine.visit_planned(&haystack, config, plan(), |matched| {
            visited.push(matched);
            visited.len() < 7
        });

        assert!(expected.len() > 1000, "{config:?}");
        assert_eq!(collected, expected, "{config:?}");
        assert_eq!(limited, expected[..5], "{config:?}");
        assert_eq!(counted, expected.len(), "{config:?}");
        assert_eq!(counted_to_limit, 5, "{config:?}");
        assert_eq!(visited, expected[..7], "{config:?}");
        assert_eq!(engine.count_planned(&haystack, config, 0, plan()), 0);
    }
}

#[test]
fn planned_searches_resume_where_the_block_scan_gives_up() {
    let engine = SinglePatternEngine::new(b"needle".to_vec());
    // Planned on text without any byte of the pattern, then run on text whose every word
    // agrees with the pattern up to its last byte.
    let quiet = vec![b'.'; PLANNED_LEN];
    let mut decoys = Vec::with_capacity(PLANNED_LEN + 64);
    for index in 0.. {
        if decoys.len() >= PLANNED_LEN {
            break;
        }
        let word: &[u8] = match index % 97 {
            0 => b"needle ",
            50 => b"NEEDLEneedle ",
            _ => b"needlE needl. ",
        };
        decoys.extend_from_slice(word);
    }
    for config in configs() {
        let expected = reference(b"needle", &decoys, config);
        let plan = || engine.block_plan(&quiet, config);
        let (scanner, scan_plan) = plan().expect("the quiet text is long enough to sample");
        let gave_up = scanner.scan(&scan_plan, &decoys, |matched| matched.end);

        let collected = engine.search_planned(&decoys, config, usize::MAX, plan());
        let counted = engine.count_planned(&decoys, config, usize::MAX, plan());
        let mut visited = Vec::new();
        engine.visit_planned(&decoys, config, plan(), |matched| {
            visited.push(matched);
            true
        });

        assert!(
            gave_up.is_some_and(|resume| resume < decoys.len() / 2),
            "{config:?}"
        );
        assert_eq!(collected, expected, "{config:?}");
        assert_eq!(counted, expected.len(), "{config:?}");
        assert_eq!(visited, expected, "{config:?}");
    }
}

#[test]
fn occurrence_counts_stop_at_the_limit() {
    let finder = memchr::memmem::Finder::new("aa");
    let haystack = b"aaaaa-aa";

    let counts = [0, 1, 3, 4].map(|limit| super::count_occurrences(&finder, haystack, limit));
    let without_match = super::count_occurrences(&finder, b"a-a", usize::MAX);

    // Non-overlapping: "aaaaa" holds two occurrences, then "aa" one more.
    assert_eq!(counts, [0, 1, 3, 3]);
    assert_eq!(without_match, 0);
}

#[test]
fn overlapping_capacity_follows_sampled_density_within_its_ceiling() {
    let engine = SinglePatternEngine::new(b"ana".to_vec());
    let config = SearchConfig::builder().overlapping(true).build();
    let dense = b"bananana".repeat(4096);
    let sparse = vec![b'x'; dense.len()];
    let found = engine.search(&dense, config, usize::MAX).len();

    let dense_capacity = engine.sampled_overlapping_capacity(&dense, config, 64);

    // Room for every match, with at most the slack on top.
    assert!((found..found + found / 8).contains(&dense_capacity));
    assert_eq!(engine.sampled_overlapping_capacity(&sparse, config, 64), 64);
    assert_eq!(
        engine.sampled_overlapping_capacity(&dense[..8 * 1024], config, 64),
        64
    );
    assert_eq!(
        SinglePatternEngine::new(b"aa".to_vec()).sampled_overlapping_capacity(
            &vec![b'a'; 4 * 1024 * 1024],
            config,
            64
        ),
        crate::profiles::OVERLAPPING_CAPACITY_CEILING
    );
}

#[test]
fn anchor_byte_is_the_first_pattern_byte_that_occurs_once() {
    let engine = SinglePatternEngine::new(b"aabca".to_vec());

    let anchor = engine.anchor_byte();

    assert_eq!(anchor, b'b');
}
