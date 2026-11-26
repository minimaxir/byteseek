use super::parallel::{boundary_is_safe, line_split_points};
use super::{Engine, SearchConfig, Searcher};

#[test]
fn non_overlapping_parallel_search_uses_semantics_safe_splits() {
    let searcher = Searcher::new(["aaaa", "aaaa"], SearchConfig::default())
        .expect("test patterns should compile");
    let mut haystack = vec![b'x'; 128 * 1024];
    let num_chunks = searcher.exact_chunk_count(haystack.len());
    let base_chunk_size = haystack.len() / num_chunks;
    for chunk_index in 1..num_chunks {
        let boundary = chunk_index * base_chunk_size;
        haystack[boundary - 1..boundary + 6].fill(b'a');
    }
    let expected = searcher.find_sequential(&haystack, usize::MAX);

    let actual = searcher.find_bytes_with_limit(&haystack, None);

    assert_eq!(actual, expected);
}

#[test]
fn ordinary_single_pattern_safe_splits_preserve_case_modes() {
    let cases = [
        (SearchConfig::default(), b"abcde".as_slice()),
        (
            SearchConfig::builder()
                .case_mode(crate::CaseMode::AsciiInsensitive)
                .build(),
            b"AbCdE".as_slice(),
        ),
    ];

    for (config, occurrence) in cases {
        let searcher = Searcher::new(["abcde"], config).expect("test pattern should compile");
        let mut haystack = vec![b'x'; 600_000];
        let num_chunks = searcher.exact_chunk_count(haystack.len());
        let base_chunk_size = haystack.len() / num_chunks;
        for chunk_index in 1..num_chunks {
            let boundary = chunk_index * base_chunk_size;
            haystack[boundary - 2..boundary + 3].copy_from_slice(occurrence);
        }
        let expected = searcher.find_sequential(&haystack, usize::MAX);

        let actual = searcher.find_bytes_with_limit(&haystack, None);

        assert!(!expected.is_empty());
        assert_eq!(actual, expected);
    }
}

#[test]
fn low_alphabet_splits_avoid_occurrences_at_chunk_boundaries() {
    let searcher =
        Searcher::new(["ACGTAC"], SearchConfig::default()).expect("test pattern should compile");
    let mut haystack = vec![b'A'; 600_000];
    let chunk_count = searcher.exact_chunk_count(haystack.len());
    let nominal_boundary = haystack.len() / chunk_count;
    haystack[nominal_boundary - 2..nominal_boundary + 4].copy_from_slice(b"ACGTAC");
    let Engine::Single(engine) = &searcher.engine else {
        panic!("one exact pattern should build a single-pattern engine");
    };

    let splits = searcher
        .exact_split_points(&haystack)
        .expect("sparse exact matches permit safe partitions");

    assert!(splits.len() > 2);
    assert!(splits.iter().copied().all(|split| boundary_is_safe(
        &haystack,
        engine.pattern_bytes(),
        split,
        searcher.config.case_mode()
    )));
    assert_eq!(searcher.find_bytes_with_limit(&haystack, None).len(), 1);
}

#[test]
fn low_alphabet_counts_equal_collected_match_counts() {
    // Limits fall both below and above the number of occurrences.
    let mut seed = 0x1234_5678_9ABC_DEF0_u64;
    let haystack = (0..700_000)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            char::from(b"ACGT"[(seed >> 32) as usize & 3])
        })
        .collect::<String>();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("a four-thread pool should build");

    for max_matches in [None, Some(7), Some(100_000)] {
        let config = SearchConfig::builder().max_matches(max_matches).build();
        let searcher = Searcher::new(["ACGTAC"], config).expect("test pattern should compile");
        let found = searcher.find(&haystack).len();
        let pooled = pool.install(|| searcher.count(&haystack));

        assert!(found > 0);
        assert_eq!(searcher.count(&haystack), found, "{max_matches:?}");
        assert_eq!(pooled, found, "{max_matches:?}");
    }
}

#[test]
fn line_splits_are_distinct_and_cover_long_and_empty_lines() {
    for text in [
        "",
        "a",
        "a\n",
        "\n\n\n",
        "long line without separators",
        "a\r\nb\nc",
    ] {
        for count in [2, 4, 32] {
            let splits = line_split_points(text.as_bytes(), count);
            assert_eq!(splits.first(), Some(&0));
            assert_eq!(splits.last(), Some(&text.len()));
            assert!(
                splits
                    .windows(2)
                    .all(|pair| pair[0] < pair[1] || text.is_empty())
            );
            assert!(
                splits[1..splits.len() - 1]
                    .iter()
                    .all(|&offset| text.as_bytes()[offset - 1] == b'\n')
            );
        }
    }
}

#[test]
fn debug_output_names_config_and_patterns_without_engine_internals() {
    let searcher =
        Searcher::new(["alpha", "beta"], SearchConfig::default()).expect("patterns compile");

    let debug = format!("{searcher:?}");

    assert!(debug.starts_with("Searcher {"));
    assert!(debug.contains("\"alpha\"") && debug.contains("\"beta\""));
    assert!(debug.ends_with(".. }"));
}

#[test]
fn engines_that_search_bytes_find_matches_beside_bytes_that_are_not_utf8() {
    let haystack = &b"x \xff needle and thread \xfe"[..];
    let builder = SearchConfig::builder;
    // Expected first-match starts; `None` where boolean or approximate matches only need to
    // lie within the haystack.
    let searchers = [
        (Searcher::new(["needle"], builder().build()), Some(4)),
        (
            Searcher::new(["needle", "thread"], builder().build()),
            Some(4),
        ),
        (
            Searcher::new(
                ["needle AND thread"],
                builder().boolean_queries(true).build(),
            ),
            None,
        ),
        (
            Searcher::new(["nedle"], builder().max_edit_distance(Some(1)).build()),
            None,
        ),
    ];
    for (searcher, exact_start) in searchers {
        let searcher = searcher.expect("test searchers compile");

        let first = searcher.find_first_in_bytes(haystack);
        let contains = searcher.contains_in_bytes(haystack);
        let absent = searcher.contains_in_bytes(b"\xff\xfe no match");

        assert!(
            first.is_some_and(|found| found.start < found.end && found.end <= haystack.len()),
            "{searcher:?}"
        );
        if let Some(start) = exact_start {
            assert_eq!(first.map(|found| found.start), Some(start), "{searcher:?}");
        }
        assert!(contains, "{searcher:?}");
        assert!(!absent, "{searcher:?}");
    }
}

#[test]
fn lines_counted_one_by_one_are_the_lines_that_text_splits_into() {
    let texts = ["", "\n", "a", "a\n", "a\r\na\n\nb\r", "\n\na\r\r\n", "b\na"];
    // `\r` is found only where line splitting keeps it, never before a line feed.
    for pattern in ["a", "\r"] {
        let searcher = Searcher::new([pattern], SearchConfig::default()).expect("pattern compiles");
        for text in texts {
            let expected = text.lines().filter(|line| line.contains(pattern)).count();

            let count = searcher.count_lines_one_by_one(text.as_bytes());

            assert_eq!(count, expected, "{pattern:?} in {text:?}");
        }
    }
}

#[test]
fn no_line_is_counted_one_by_one_under_a_match_limit_of_zero() {
    let none = SearchConfig::builder().max_matches(Some(0)).build();
    let searcher = Searcher::new(["a"], none).expect("pattern compiles");

    let count = searcher.count_lines_one_by_one(b"a\na\n");

    assert_eq!(count, 0);
}

#[test]
fn one_thread_pools_search_sequentially_with_identical_results() {
    let searcher =
        Searcher::new(["ACGTAC"], SearchConfig::default()).expect("test pattern should compile");
    let mut seed = 0x1234_5678_9ABC_DEF0_u64;
    let haystack = (0..1_200_000)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            b"ACGT"[(seed >> 62) as usize]
        })
        .collect::<Vec<_>>();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("a one-thread pool should build");

    // `install` runs the closure inside the pool, so rayon reports one thread there.
    let (workers, sequential) = pool.install(|| {
        (
            crate::profiles::has_parallel_workers(),
            searcher.find_bytes_with_limit(&haystack, None),
        )
    });

    assert!(!workers);
    assert!(!sequential.is_empty());
    assert_eq!(sequential, searcher.find_bytes_with_limit(&haystack, None));
}

#[test]
fn shared_boolean_plan_counts_chunks_like_the_whole_haystack() {
    let config = SearchConfig::builder().boolean_queries(true).build();
    let searcher = Searcher::new(["ERROR AND status=500 OR (WARN AND NOT retry)"], config)
        .expect("boolean query should compile");
    let haystack = (0..8_000)
        .map(|index| {
            let level = ["INFO", "WARN", "ERROR"][index % 3];
            let status = if index % 7 == 0 { 500 } else { 200 };
            let retry = if index % 5 == 0 { " retry" } else { "" };
            format!("{index} {level} status={status}{retry}\n")
        })
        .collect::<String>();
    let Engine::Boolean(engine) = &searcher.engine else {
        panic!("boolean queries should build a boolean engine");
    };
    let bytes = haystack.as_bytes();

    let plan = engine
        .line_count_plan(bytes, config)
        .expect("every clause has a positive anchor term");
    let splits = line_split_points(bytes, 5);
    let chunked = splits
        .windows(2)
        .map(|window| engine.count_planned_lines(&bytes[window[0]..window[1]], config, &plan))
        .sum::<usize>();

    assert_eq!(chunked, engine.count_matching_lines(bytes, config));
    assert_eq!(
        chunked,
        haystack
            .lines()
            .filter(|line| searcher.contains(line))
            .count()
    );
}

#[test]
fn one_thread_approximate_regions_match_parallel_regions() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = Searcher::new(["needle"], config).expect("approximate pattern compiles");
    let haystack = (0..250_000)
        .map(|index| match index % 5 {
            0 => "a needle here\n",
            1 => "a needel there\n",
            2 => "no match line.\n",
            3 => "neddle and nedle\n",
            _ => "plain filler text\n",
        })
        .collect::<String>();
    assert!(haystack.len() > 3 * 1024 * 1024);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("a one-thread pool should build");

    let (sequential, first) =
        pool.install(|| (searcher.find(&haystack), searcher.find_first(&haystack)));

    assert!(!sequential.is_empty());
    assert_eq!(sequential, searcher.find(&haystack));
    assert_eq!(first, sequential.first().copied());
}

#[test]
fn parallel_counts_equal_collected_match_counts() {
    let case_insensitive = SearchConfig::builder().case_mode(crate::CaseMode::AsciiInsensitive);
    let configs = [
        SearchConfig::default(),
        case_insensitive.build(),
        case_insensitive.whole_word(true).build(),
        case_insensitive.overlapping(true).build(),
        case_insensitive.max_matches(Some(1_000)).build(),
    ];
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("a four-thread pool should build");
    // Mixed-case occurrences everywhere, including chunk boundaries, some inside words.
    let mut haystack = b"Abab abab. ABABx ".repeat(40_000);
    let searcher = Searcher::new(["abab"], SearchConfig::default()).expect("pattern compiles");
    let num_chunks = pool.install(|| searcher.exact_chunk_count(haystack.len()));
    for chunk_index in 1..num_chunks {
        let boundary = chunk_index * (haystack.len() / num_chunks);
        haystack[boundary - 2..boundary + 2].copy_from_slice(b"aBAb");
    }
    let haystack = String::from_utf8(haystack).expect("ASCII is UTF-8");

    for config in configs {
        for patterns in [&["abab"][..], &["abab", "ab. "]] {
            let searcher = Searcher::new(patterns.iter().copied(), config)
                .expect("test patterns should compile");

            let (counted, found) =
                pool.install(|| (searcher.count(&haystack), searcher.find(&haystack).len()));

            assert!(found > 0, "{config:?} {patterns:?}");
            assert_eq!(counted, found, "{config:?} {patterns:?}");
        }
    }
}
