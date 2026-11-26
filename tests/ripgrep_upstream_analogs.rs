use byteseek::{CaseMode, LineMatch, SearchConfig, Searcher};

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
    Searcher::new(patterns.iter().copied(), config).expect("valid ripgrep analog patterns")
}

fn build_searcher_from_pattern_file(pattern_file: &str, config: SearchConfig) -> Searcher {
    let patterns = pattern_file.lines().collect::<Vec<_>>();
    Searcher::new(patterns.iter().copied(), config).expect("valid pattern file contents")
}

fn line_numbers(lines: &[LineMatch<'_>]) -> Vec<usize> {
    lines.iter().map(|line| line.line_number).collect()
}

fn matched_texts<'a>(lines: &'a [LineMatch<'a>]) -> Vec<&'a str> {
    lines
        .iter()
        .flat_map(|line| {
            line.matches
                .iter()
                .map(|matched| &line.line[matched.start..matched.end])
        })
        .collect()
}

#[test]
fn f7() {
    let searcher = build_searcher(&["Sherlock", "Holmes"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(line_numbers(&lines), vec![1, 2, 3]);
}

#[test]
fn f7_stdin() {
    let searcher = build_searcher_from_pattern_file("Sherlock\nHolmes", SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(line_numbers(&lines), vec![1, 2, 3]);
}

#[test]
fn f20_no_filename() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(
        lines[0].line,
        SHERLOCK.lines().next().expect("line 1 exists")
    );
    assert_eq!(
        lines[1].line,
        SHERLOCK.lines().nth(2).expect("line 3 exists")
    );
}

#[test]
fn f34_only_matching() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(matched_texts(&lines), vec!["Sherlock", "Sherlock"]);
}

#[test]
fn f34_only_matching_line_column() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(line_numbers(&lines), vec![1, 3]);
    assert_eq!(lines[0].matches[0].start, 56);
    assert_eq!(lines[0].matches[0].end, 64);
    assert_eq!(lines[1].matches[0].start, 48);
    assert_eq!(lines[1].matches[0].end, 56);
}

#[test]
fn f89_count() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());

    assert_eq!(searcher.count_matching_lines(SHERLOCK), 2);
    assert_eq!(searcher.count(SHERLOCK), 2);
}

#[test]
fn f89_files_with_matches() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());

    assert!(searcher.contains(SHERLOCK));
}

#[test]
fn f89_files_without_match() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());

    assert!(!searcher.contains("foo"));
}

#[test]
fn f159_max_count() {
    let config = SearchConfig::builder().max_matches(Some(1)).build();
    let searcher = build_searcher(&["test"], config);

    assert_eq!(searcher.find("test\ntest").len(), 1);
    assert_eq!(searcher.count("test\ntest"), 1);
}

#[test]
fn f159_max_count_zero() {
    let config = SearchConfig::builder().max_matches(Some(0)).build();
    let searcher = build_searcher(&["test"], config);

    assert!(searcher.find("test\ntest").is_empty());
    assert_eq!(searcher.count("test\ntest"), 0);
    assert_eq!(searcher.find_first("test\ntest"), None);
    assert!(!searcher.contains("test\ntest"));
}

#[test]
fn f416_crlf() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK_CRLF);

    assert_eq!(line_numbers(&lines), vec![1, 3]);
    assert!(lines.iter().all(|line| !line.line.ends_with('\r')));
    assert_eq!(matched_texts(&lines), vec!["Sherlock", "Sherlock"]);
}

#[test]
fn f416_crlf_only_matching() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK_CRLF);

    assert_eq!(matched_texts(&lines), vec!["Sherlock", "Sherlock"]);
}

#[test]
fn basic1() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(line_numbers(&lines), vec![1, 3]);
}

#[test]
fn basic2() {
    let searcher = build_searcher(&["NADA"], SearchConfig::default());

    assert!(!searcher.contains(SHERLOCK));
    assert!(searcher.find(SHERLOCK).is_empty());
    assert!(searcher.find_lines(SHERLOCK).is_empty());
    assert_eq!(searcher.count_matching_lines(SHERLOCK), 0);
}

#[test]
fn basic3() {
    let searcher = build_searcher(&["a"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(line_numbers(&lines), vec![1, 2, 3, 4, 5, 6]);
    assert!(lines.iter().all(|line| !line.matches.is_empty()));
}

#[test]
fn basic4() {
    let searcher = build_searcher(&["a"], SearchConfig::default());
    let haystack = "a\nb\n\nc\n\n\nd\n";
    let lines = searcher.find_lines(haystack);

    assert_eq!(line_numbers(&lines), vec![1]);
    assert_eq!(matched_texts(&lines), vec!["a"]);
}

#[test]
fn line_number1() {
    let searcher = build_searcher(&["Sherlock"], SearchConfig::default());
    let lines = searcher.find_lines(SHERLOCK);

    assert_eq!(line_numbers(&lines), vec![1, 3]);
}

#[test]
fn big1() {
    let repeat_count = 300_000_usize;
    let mut haystack = String::with_capacity(2 + repeat_count * 4 + 2);
    haystack.push_str("a\n");
    for _ in 0..repeat_count {
        haystack.push_str("zzz\n");
    }
    haystack.push_str("a\n");

    let searcher = build_searcher(&["a"], SearchConfig::default());
    let lines = searcher.find_lines_parallel(haystack.as_str());

    assert_eq!(line_numbers(&lines), vec![1, repeat_count + 2]);
    assert_eq!(matched_texts(&lines), vec!["a", "a"]);
}

#[test]
fn fused_multi_pattern_lines_preserve_crlf_order_and_chunk_offsets() {
    let mut haystack = String::with_capacity(1_200_000);
    let mut expected_line_numbers = Vec::new();
    for index in 0..180_000_usize {
        if index % 40_000 == 0 {
            haystack.push_str("AbC\r\n");
            expected_line_numbers.push(index + 1);
        } else {
            haystack.push_str("none\r\n");
        }
    }

    let searcher = build_searcher(
        &["a", "abc", "bc"],
        SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .build(),
    );
    let lines = searcher.find_lines_parallel(haystack.as_str());

    assert_eq!(line_numbers(&lines), expected_line_numbers);
    assert!(lines.iter().all(|line| line.line == "AbC"));
    assert!(lines.iter().all(|line| {
        line.matches.as_slice()
            == [byteseek::Match {
                start: 0,
                end: 3,
                pattern_index: 1,
            }]
    }));
}

#[test]
fn parallel_lines_fall_back_for_limits_and_newline_patterns() {
    let haystack = "a\nb\n".repeat(150_000);
    let limited = build_searcher(
        &["a", "aa"],
        SearchConfig::builder().max_matches(Some(1)).build(),
    );
    assert_eq!(
        limited.find_lines_parallel(haystack.as_str()),
        limited.find_lines(haystack.as_str())
    );

    let crlf_haystack = "a\r\nb\r\n".repeat(150_000);
    let newline = build_searcher(&["a\r", "b"], SearchConfig::default());
    assert_eq!(
        newline.find_lines_parallel(crlf_haystack.as_str()),
        newline.find_lines(crlf_haystack.as_str())
    );

    let mode_haystack = "word sword ana banana\n".repeat(40_000);
    for config in [
        SearchConfig::builder().whole_word(true).build(),
        SearchConfig::builder().overlapping(true).build(),
    ] {
        let parallel = build_searcher(&["word", "ana"], config);
        let reference = build_searcher(
            &["word", "ana"],
            SearchConfig::builder()
                .whole_word(config.whole_word())
                .overlapping(config.overlapping())
                .max_matches(Some(usize::MAX))
                .build(),
        );
        assert_eq!(
            parallel.find_lines_parallel(mode_haystack.as_str()),
            reference.find_lines(mode_haystack.as_str())
        );
    }
}

#[test]
fn regression_2260() {
    let searcher = build_searcher(&["GATC"], SearchConfig::default());
    let lines = searcher.find_lines("GATC\n");

    assert_eq!(line_numbers(&lines), vec![1]);
    assert_eq!(matched_texts(&lines), vec!["GATC"]);
}

#[test]
fn r506_word_not_parenthesized() {
    let config = SearchConfig::builder().whole_word(true).build();
    let searcher = build_searcher(&["min", "max"], config);
    let lines = searcher.find_lines("min minimum amin\nmax maximum amax");

    assert_eq!(line_numbers(&lines), vec![1, 2]);
    assert_eq!(matched_texts(&lines), vec!["min", "max"]);
}

#[test]
fn r493() {
    let config = SearchConfig::builder().whole_word(true).build();
    let searcher = build_searcher(&["'re"], config);
    let lines = searcher.find_lines("peshwaship 're seminomata");

    assert_eq!(matched_texts(&lines), vec!["'re"]);
}

#[test]
fn r553_switch() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = build_searcher(&["sherlock"], config);

    let first = searcher.find_lines(SHERLOCK);
    let second = searcher.find_lines(SHERLOCK);

    assert_eq!(first, second);
    assert_eq!(line_numbers(&first), vec![1, 3]);
}

#[test]
fn r1176_literal_file() {
    let searcher = build_searcher(&["foo(bar"], SearchConfig::default());
    let lines = searcher.find_lines("foo(bar\n");

    assert_eq!(line_numbers(&lines), vec![1]);
    assert_eq!(matched_texts(&lines), vec!["foo(bar"]);
}

#[test]
fn r1203_reverse_suffix_literal() {
    let haystack = "153.230000\n";
    let first = build_searcher(&["230000"], SearchConfig::default());
    let second = build_searcher(&["30000"], SearchConfig::default());

    assert!(first.contains(haystack));
    assert!(second.contains(haystack));
}

#[test]
fn r1259_drop_last_byte_nonl() {
    let haystack = "fz";
    let without_newline = build_searcher_from_pattern_file("fz", SearchConfig::default());
    let with_newline = build_searcher_from_pattern_file("fz\n", SearchConfig::default());

    assert!(without_newline.contains(haystack));
    assert!(with_newline.contains(haystack));
}

#[test]
fn r1334_crazy_literals() {
    let patterns = vec!["1.208.0.0/12"; 40];
    let searcher = Searcher::new(patterns.iter().copied(), SearchConfig::default())
        .expect("duplicate literal set should compile");
    let lines = searcher.find_lines("1.208.0.0/12\n");

    assert_eq!(line_numbers(&lines), vec![1]);
    assert_eq!(matched_texts(&lines), vec!["1.208.0.0/12"]);
}
