use byteseek::{Match, SearchConfig, SearchError, Searcher};

fn build_searcher(patterns: &[&str], config: SearchConfig) -> Searcher {
    Searcher::new(patterns.iter().copied(), config).expect("searcher should build")
}

#[test]
fn boolean_queries_are_opt_in_for_plain_text_search() {
    let haystack = "time AND date\ndate before time\n";

    let literal = build_searcher(&["time AND date"], SearchConfig::default());
    assert_eq!(
        literal
            .find_lines(haystack)
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>(),
        vec!["time AND date"]
    );

    let boolean = build_searcher(
        &["time AND date"],
        SearchConfig::builder().boolean_queries(true).build(),
    );
    assert_eq!(
        boolean
            .find_lines(haystack)
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>(),
        vec!["time AND date", "date before time"]
    );
}

#[test]
fn boolean_and_matches_are_order_insensitive_per_line() {
    let haystack = "time before date\ndate before time\ntime only\n";
    let searcher = build_searcher(
        &["time AND date"],
        SearchConfig::builder().boolean_queries(true).build(),
    );

    let lines = searcher.find_lines(haystack);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].line, "time before date");
    assert_eq!(lines[1].line, "date before time");
    let span = |start, end| Match {
        start,
        end,
        pattern_index: 0,
    };
    // Matches are the query's terms, wherever they are on the line.
    assert_eq!(lines[0].matches.as_slice(), [span(0, 4), span(12, 16)]);
    assert_eq!(lines[1].matches.as_slice(), [span(0, 4), span(12, 16)]);
}

#[test]
fn matches_are_positive_terms_or_the_line_a_negation_selects() {
    let config = SearchConfig::builder().boolean_queries(true).build();
    let searcher = build_searcher(&["ERROR AND NOT retry", "NOT ok"], config);
    let haystack = "ERROR x ERROR
ERROR retry
ok
warn
";
    let span = |start, end, pattern_index| Match {
        start,
        end,
        pattern_index,
    };

    let found = searcher.find(haystack);

    // The first line holds both queries; only the first query has terms to show. The last
    // line holds only through `NOT ok`, so the whole line is its match.
    assert_eq!(
        found,
        [
            span(0, 5, 0),
            span(8, 13, 0),
            span(14, 25, 1),
            span(29, 33, 1)
        ]
    );
    assert_eq!(searcher.find_first(haystack), Some(span(0, 5, 0)));
    assert_eq!(searcher.count(haystack), 4);
    assert_eq!(searcher.count_matching_lines(haystack), 3);
}

#[test]
fn overlapping_terms_of_several_queries_keep_the_leftmost_unless_overlapping() {
    let queries = ["cat AND category", "category AND NOT dog"];
    let span = |start, end, pattern_index| Match {
        start,
        end,
        pattern_index,
    };
    let plain = build_searcher(
        &queries,
        SearchConfig::builder().boolean_queries(true).build(),
    );
    let overlapping = build_searcher(
        &queries,
        SearchConfig::builder()
            .boolean_queries(true)
            .overlapping(true)
            .build(),
    );

    assert_eq!(plain.find("category"), [span(0, 8, 0)]);
    assert_eq!(
        overlapping.find("category"),
        [span(0, 8, 0), span(0, 3, 0), span(0, 8, 1)]
    );
}

#[test]
fn boolean_queries_support_nested_logic_and_not_clauses() {
    let haystack = concat!(
        "2026 INFO service=auth status=200 message=handled\n",
        "2026 ERROR service=auth status=500 message=handled\n",
        "2026 WARN service=billing status=200 message=handled\n",
        "2026 WARN service=auth status=200 ignored\n",
    );
    let query = "((ERROR AND status=500) OR (WARN AND service=billing)) AND NOT ignored";
    let searcher = build_searcher(
        &[query],
        SearchConfig::builder().boolean_queries(true).build(),
    );

    let lines = searcher.find_lines(haystack);
    assert_eq!(
        lines.into_iter().map(|line| line.line).collect::<Vec<_>>(),
        vec![
            "2026 ERROR service=auth status=500 message=handled",
            "2026 WARN service=billing status=200 message=handled",
        ]
    );
}

#[test]
fn boolean_queries_support_both_quote_styles_with_whitespace() {
    let haystack = concat!(
        "clock date stamp event\n",
        "date stamp only\n",
        "clock only\n",
        "clock\n",
    );
    let searcher = build_searcher(
        &[r#""date stamp" AND clock OR 'clock only'"#],
        SearchConfig::builder().boolean_queries(true).build(),
    );

    assert_eq!(
        searcher
            .find_lines(haystack)
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>(),
        vec!["clock date stamp event", "clock only"]
    );
}

#[test]
fn boolean_queries_respect_not_and_or_precedence() {
    let haystack = "alpha\nbeta\nbeta gamma\nalpha gamma\ngamma\n";
    let searcher = build_searcher(
        &["alpha OR beta AND NOT gamma"],
        SearchConfig::builder().boolean_queries(true).build(),
    );

    assert_eq!(
        searcher
            .find_lines(haystack)
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>(),
        vec!["alpha", "beta", "alpha gamma"]
    );
}

#[test]
fn boolean_queries_reject_malformed_expressions() {
    let error = match Searcher::new(
        ["time AND (date OR"],
        SearchConfig::builder().boolean_queries(true).build(),
    ) {
        Ok(_) => panic!("malformed boolean query should fail"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        SearchError::InvalidBooleanPattern {
            index: 0,
            message: "boolean query is missing a literal or parenthesized clause".to_owned(),
        }
    );
}

/// Reference evaluator for one query, given a line-local term-presence test.
type LineOracle = fn(&dyn Fn(&str) -> bool) -> bool;

#[test]
fn sampled_clause_planning_preserves_line_counts() {
    const WORDS: [&str; 6] = ["ERROR", "WARN", "status=500", "status=200", "auth", "TRACE"];
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut haystack = String::new();
    let mut lines = Vec::new();
    // Large enough (about 1 MB) for parallel line counting, whose chunks share one plan.
    for _ in 0..40_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let line = WORDS
            .iter()
            .enumerate()
            .filter(|(index, _)| (state >> (index * 3)) & 0b111 < 3)
            .map(|(_, word)| *word)
            .collect::<Vec<_>>()
            .join(" filler ");
        haystack.push_str(&line);
        haystack.push_str(if state.is_multiple_of(5) {
            "\r\n"
        } else {
            "\n"
        });
        lines.push(line);
    }
    let has = |line: &str, term: &str| line.contains(term);
    let queries: [(&str, LineOracle); 4] = [
        ("ERROR AND status=500", |t| t("ERROR") && t("status=500")),
        ("status=200 AND WARN AND NOT TRACE", |t| {
            t("status=200") && t("WARN") && !t("TRACE")
        }),
        (
            "((ERROR AND status=500) OR (WARN AND status=200)) AND auth AND NOT TRACE",
            |t| {
                ((t("ERROR") && t("status=500")) || (t("WARN") && t("status=200")))
                    && t("auth")
                    && !t("TRACE")
            },
        ),
        ("auth AND NOT auth", |t| t("auth") && !t("auth")),
    ];
    let config = SearchConfig::builder().boolean_queries(true).build();

    for (query, oracle) in queries {
        let expected = lines
            .iter()
            .filter(|line| oracle(&|term| has(line, term)))
            .count();
        let searcher = build_searcher(&[query], config);
        assert_eq!(
            searcher.count_matching_lines(&haystack),
            expected,
            "{query}"
        );
        assert_eq!(searcher.find_lines(&haystack).len(), expected, "{query}");
    }
}

#[test]
fn deep_and_wide_boolean_queries_stay_bounded() {
    let config = SearchConfig::builder().boolean_queries(true).build();
    let too_deep = [
        format!("{}a{}", "(".repeat(100_000), ")".repeat(100_000)),
        format!("{}a", "NOT ".repeat(100_000)),
        vec!["a"; 100_000].join(" OR "),
    ];
    for query in &too_deep {
        assert!(matches!(
            Searcher::new([query.as_str()], config),
            Err(SearchError::InvalidBooleanPattern { index: 0, .. })
        ));
    }
    let nested = format!("{}a{}", "(".repeat(100), ")".repeat(100));
    assert_eq!(
        build_searcher(&[&nested], config).count_matching_lines("a\nb\n"),
        1
    );

    // Sixteen million clauses if every conjunction were multiplied out.
    let wide = (0..24)
        .map(|index| format!("(a{index}x OR b{index}x)"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let all_left = (0..24)
        .map(|index| format!("a{index}x"))
        .collect::<Vec<_>>()
        .join(" ");
    let haystack = format!("{all_left}\na0x b1x\n{all_left} tail\n");
    let searcher = build_searcher(&[&wide], config);

    assert_eq!(searcher.count_matching_lines(&haystack), 2);
    assert_eq!(searcher.find_lines(&haystack).len(), 2);
}

#[test]
fn quoted_boolean_literals_keep_text_beyond_ascii() {
    let config = SearchConfig::builder().boolean_queries(true).build();
    let searcher = build_searcher(&["\"café au lait\" AND NOT \"thé\\\"vert\""], config);
    let haystack = "un café au lait\ncafe au lait\ncafé au lait et thé\"vert\n";

    let lines = searcher
        .find_lines(haystack)
        .into_iter()
        .map(|line| line.line_number)
        .collect::<Vec<_>>();

    assert_eq!(lines, [1]);
    assert_eq!(searcher.count_matching_lines(haystack), 1);
}

#[test]
fn terms_that_share_bytes_are_each_found_where_they_overlap() {
    let config = SearchConfig::builder().boolean_queries(true).build();
    let cases = [
        ("cat AND category", "category\ncat\n", 1),
        ("abc AND bcd", "abcd\nabc bcd\nabc\n", 2),
        ("(cat AND category) OR NOT zzz", "category\nzzz\n", 1),
    ];

    for (query, haystack, matching_lines) in cases {
        let searcher = build_searcher(&[query], config);

        assert_eq!(
            searcher.find_lines(haystack).len(),
            matching_lines,
            "{query}"
        );
        assert_eq!(
            searcher.count_matching_lines(haystack),
            matching_lines,
            "{query}"
        );
        assert_eq!(searcher.contains(haystack), matching_lines > 0, "{query}");
    }
    // Where two terms of one query overlap, the leftmost is the match.
    let searcher = build_searcher(&["abc AND bcd"], config);
    assert_eq!(
        searcher
            .find("abcd\nabc bcd\nabc\n")
            .iter()
            .map(|found| (found.start, found.end))
            .collect::<Vec<_>>(),
        [(0, 3), (5, 8), (9, 12)]
    );
}
