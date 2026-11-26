use std::num::NonZeroUsize;

use super::plan::{MIN_WEIGHED_SHARE_LEN, PackedWork, RankedPair, RankedPairs};
use super::*;

/// A whole haystack searched by one thread; scans are weighed only if it is long.
const WHOLE: Extent = Extent::ON_ONE_THREAD;
/// A piece late enough in its input for scans to be weighed at any length.
const LATER_PIECE: Extent = Extent::Piece {
    preceded_by: MIN_WEIGHED_SHARE_LEN,
};

/// Counts matching lines by searching every line for every pattern.
fn reference(patterns: &[&str], haystack: &[u8], insensitive: bool) -> usize {
    haystack
        .split(|&byte| byte == b'\n')
        .filter(|line| {
            patterns.iter().any(|pattern| {
                line.windows(pattern.len()).any(|window| {
                    if insensitive {
                        window.eq_ignore_ascii_case(pattern.as_bytes())
                    } else {
                        window == pattern.as_bytes()
                    }
                })
            })
        })
        .count()
}

fn owned(patterns: &[&str]) -> Vec<Vec<u8>> {
    patterns
        .iter()
        .map(|pattern| pattern.as_bytes().to_vec())
        .collect()
}

fn case_config(case_mode: CaseMode) -> SearchConfig {
    SearchConfig::builder().case_mode(case_mode).build()
}

fn scanner(patterns: &[&str], case_mode: CaseMode) -> PairScanner {
    PairScanner::new(&owned(patterns), case_config(case_mode), None)
        .expect("the patterns are scannable")
}

/// A plan's anchors: `leading`, then zeros.
fn anchors(leading: &[usize]) -> [usize; MAX_PATTERNS] {
    std::array::from_fn(|index| leading.get(index).copied().unwrap_or(0))
}

/// Counts with the scanner, finishing with the reference search.
fn finished(scanner: &PairScanner, plan: &PairPlan, patterns: &[&str], haystack: &[u8]) -> usize {
    let (count, unscanned) = scanner.count_lines(plan, haystack);
    count + reference(patterns, &haystack[unscanned..], scanner.insensitive)
}

/// Log lines of varying length and case; every 50th is several blocks long.
fn log(lines: usize) -> Vec<u8> {
    let mut text = Vec::new();
    for index in 0..lines {
        let level = ["INFO", "debug", "WARN", "Error", "TRACE", "warn", "ERROR"][index % 7];
        let status = if index % 17 == 0 { 500 } else { 200 };
        let padding = "x".repeat(if index % 50 == 0 { 700 } else { index % 23 });
        text.extend_from_slice(
            format!("{index:05} {level} request_id={index} status={status} {padding}\n").as_bytes(),
        );
    }
    text
}

/// Tokens no other test text holds, for the aarch64-only large-set tests.
#[cfg(target_arch = "aarch64")]
const TOKENS: [&str; 8] = [
    "ALPHA9", "BRAVO8", "CHARLIE7", "DELTA6", "ECHO5", "FOXTROT4", "GOLF3", "HOTEL2",
];

/// 4,000 lowercase lines; `count` of them, from line `first` every `spacing` lines, start
/// with the next of `tokens`.
#[cfg(target_arch = "aarch64")]
fn lines_with_tokens(tokens: &[&str], first: usize, spacing: usize, count: usize) -> Vec<u8> {
    let mut text = Vec::new();
    for line in 0..4_000_usize {
        let placed = line
            .checked_sub(first)
            .filter(|&after| after.is_multiple_of(spacing) && after / spacing < count);
        if let Some(after) = placed {
            text.extend_from_slice(tokens[(after / spacing) % tokens.len()].as_bytes());
            text.push(b' ');
        }
        text.extend_from_slice(b"the quick brown fox jumps over the lazy dog and keeps on ");
        text.extend_from_slice(b"running through fields\n");
    }
    text
}

const CASE_MODES: [CaseMode; 2] = [CaseMode::Sensitive, CaseMode::AsciiInsensitive];

#[test]
fn counts_equal_a_line_by_line_search() {
    let haystack = log(6_000);
    let pattern_sets: [&[&str]; 7] = [
        &["ERROR"],
        &["request_id=1999"],
        &["status=500", "WARN"],
        &["error", "trace", "status=500"],
        &["id=59", "status=500", "TRACE", "0 debug"],
        &["absent pattern"],
        // The second pattern occurs only in lines the first already counted.
        &["00 ", "0 INFO"],
    ];
    for patterns in pattern_sets {
        for case_mode in CASE_MODES {
            let scanner = scanner(patterns, case_mode);
            let plan = scanner.plan(&haystack, WHOLE).expect("the pairs are rare");

            let count = finished(&scanner, &plan, patterns, &haystack);

            assert_eq!(
                count,
                reference(patterns, &haystack, scanner.insensitive),
                "{patterns:?} {case_mode:?}"
            );
        }
    }
}

#[test]
fn sets_of_every_size_count_the_lines_a_line_by_line_search_counts() {
    // Pair first bytes differ in either case, so any prefix of the set keys on them.
    const PATTERNS: [(&str, usize); 8] = [
        ("ERROR", 0),
        ("WARN", 0),
        ("status=500", 7),
        ("id=59", 2),
        ("TRACE", 0),
        ("0 debug", 2),
        ("INFO", 0),
        ("Error", 1),
    ];
    let haystack = log(6_000);
    for set_len in 1..=MAX_PATTERNS {
        let patterns: Vec<&str> = PATTERNS[..set_len].iter().map(|&(text, _)| text).collect();
        let offsets: Vec<usize> = PATTERNS[..set_len].iter().map(|&(_, at)| at).collect();
        for case_mode in CASE_MODES {
            let scanner = scanner(&patterns, case_mode);
            let plan = PairPlan::new(&scanner, 1, anchors(&offsets), 0xFF)
                .expect("the first bytes of the pairs differ");

            let (scanned, unscanned) = scanner.count_lines(&plan, &haystack);
            let rest = reference(&patterns, &haystack[unscanned..], scanner.insensitive);

            assert_eq!(plan.pair_count, set_len);
            // The scan reached the end, leaving only the tail and a possible straddler.
            assert!(
                haystack.len() - unscanned < BLOCK + MAX_DISTANCE + HEAD_LEN + scanner.longest,
                "{patterns:?} {case_mode:?}"
            );
            assert_eq!(
                scanned + rest,
                reference(&patterns, &haystack, scanner.insensitive),
                "{patterns:?} {case_mode:?}"
            );
        }
    }
}

/// A window of dots with `texts` written at their offsets.
fn window(texts: &[(usize, &str)]) -> SampleWindow {
    let mut window = [b'.'; SAMPLE_WINDOW_LEN];
    for &(start, text) in texts {
        window[start..start + text.len()].copy_from_slice(text.as_bytes());
    }
    window
}

#[test]
fn packed_work_is_a_stop_per_matching_line_and_the_bytes_from_its_first_match() {
    let sample = [
        // Two matches in a line, a line without, and a match in a line leaving the window.
        window(&[
            (10, "error"),
            (20, "warn"),
            (49, "\n"),
            (99, "\n"),
            (250, "warn"),
        ]),
        // A match split across windows, which is not found.
        window(&[(127, "\n"), (253, "err")]),
        window(&[(0, "or"), (100, "\n")]),
        // A match in the line begun before the window, and one in a line of its own.
        window(&[(30, "warn"), (59, "\n"), (70, "error"), (89, "\n")]),
    ];
    let endless = [window(&[(0, "error")])];
    let one_line = [window(&[(100, "warn")]), window(&[(100, "error")])];
    // Windows are lowercase, as when sampled for insensitive matching.
    let scanners = [
        scanner(&["error", "warn"], CaseMode::Sensitive),
        scanner(&["ERROR", "Warn"], CaseMode::AsciiInsensitive),
    ];
    for scanner in scanners {
        let work = scanner.sampled_packed_work(&sample);
        let endless_work = scanner.sampled_packed_work(&endless);
        let one_line_work = scanner.sampled_packed_work(&one_line);

        // 40 bytes of the first line; 6 bytes plus the average line length (1,024 / 6)
        // for the line leaving the window; 60 bytes of the last window's partial line,
        // with no stop; 20 of its own line.
        assert_eq!(
            work,
            PackedWork {
                stops: 3,
                skipped: 40 + 6 + 170 + 60 + 20
            }
        );
        // Skipping is capped at the sample's length.
        assert_eq!(
            endless_work,
            PackedWork {
                stops: 1,
                skipped: SAMPLE_WINDOW_LEN
            }
        );
        // A window inside a line adds no stop.
        assert_eq!(
            one_line_work,
            PackedWork {
                stops: 1,
                skipped: 2 * SAMPLE_WINDOW_LEN
            }
        );
    }
}

#[test]
fn compare_operations_grow_by_two_per_pair_and_by_two_when_folding() {
    let operations = [
        PairPlan::compare_operations(1, false),
        PairPlan::compare_operations(1, true),
        PairPlan::compare_operations(8, false),
    ];

    assert_eq!(operations, [4, 6, 18]);
}

#[test]
fn scans_afford_candidates_by_what_packed_search_spends() {
    let exact = scanner(&["ab"], CaseMode::Sensitive);
    let folded = scanner(&["ab"], CaseMode::AsciiInsensitive);
    let windows = MAX_SAMPLE_WINDOWS;
    let idle = PackedWork {
        stops: 0,
        skipped: 0,
    };
    let stopping = PackedWork {
        stops: 20,
        skipped: 0,
    };
    let skipping = PackedWork {
        stops: 2,
        skipped: windows * SAMPLE_WINDOW_LEN,
    };
    let unfolded = |pairs: usize| PairPlan::compare_operations(pairs, false);

    let five = exact.affordable_candidates(unfolded(5), idle, windows);
    let six = exact.affordable_candidates(unfolded(6), idle, windows);
    let eight = exact.affordable_candidates(unfolded(8), idle, windows);
    let eight_folded =
        folded.affordable_candidates(PairPlan::compare_operations(8, true), idle, windows);
    let eight_stopping = exact.affordable_candidates(unfolded(8), stopping, windows);
    let five_skipping = exact.affordable_candidates(unfolded(5), skipping, windows);

    // Fewer pairs leave more for candidates. `None` compares below every `Some`.
    assert!(six.is_some() && five > six);
    // Eight exact pairs lose to packed search; eight folded pairs (against folding packed
    // search) pay only without candidates.
    assert_eq!(eight, None);
    assert_eq!(eight_folded, Some(0));
    // Stops favor the scan; skipped lines favor packed search.
    assert!(eight_stopping > Some(stopping.stops));
    assert_eq!(five_skipping, None);
}

/// Ranked pairs from `(occurrences, offset)` tuples, in the order given.
fn ranked(pairs: &[(usize, usize)]) -> RankedPairs {
    pairs
        .iter()
        .map(|&(occurrences, offset)| RankedPair {
            occurrences,
            byte_occurrences: 0,
            offset,
        })
        .collect()
}

#[test]
fn pairs_are_chosen_rarest_first_among_those_an_end_tells_apart() {
    let colliding = scanner(&["abcd", "aefx", "gbcd"], CaseMode::Sensitive);
    // At distance 1: `ab` and `bc`; `ae`, `fx` and `ef`; `gb`, `bc` and `cd`.
    let pairs = [
        ranked(&[(0, 0), (2, 1)]),
        ranked(&[(0, 0), (1, 2), (5, 1)]),
        ranked(&[(0, 0), (2, 1), (3, 2)]),
    ];
    let sharing = scanner(&["xbcd", "ybcd"], CaseMode::Sensitive);
    let shared = [ranked(&[(4, 1)]), ranked(&[(4, 1)])];
    let stuck = scanner(&["abcd", "aefx"], CaseMode::Sensitive);
    let single = [ranked(&[(0, 0)]), ranked(&[(0, 0)])];

    let rarest = colliding.choose_pairs(1, &pairs, None);
    let by_first = colliding.choose_pairs(1, &pairs, Some(PairEnd::First));
    let by_second = colliding.choose_pairs(1, &pairs, Some(PairEnd::Second));
    let same_pair = sharing.choose_pairs(1, &shared, Some(PairEnd::First));
    let no_pair_left = stuck.choose_pairs(1, &single, Some(PairEnd::First));

    assert_eq!(rarest, Some((anchors(&[0, 0, 0]), 0)));
    // `ae` begins like `ab`, so the second pattern takes `fx`, its next rarest.
    assert_eq!(by_first, Some((anchors(&[0, 2, 0]), 1)));
    // `gb` ends like `ab`, so the third pattern takes `bc`.
    assert_eq!(by_second, Some((anchors(&[0, 0, 1]), 2)));
    // Patterns may share a pair: the plan compares it once for both.
    assert_eq!(same_pair, Some((anchors(&[1, 1]), 8)));
    assert_eq!(no_pair_left, None);
}

#[test]
fn colliding_rarest_pairs_are_replaced_by_the_rarer_choice_only_if_it_is_affordable() {
    let scanner = scanner(&["abcd", "aefx", "gbcd"], CaseMode::Sensitive);
    let pairs = [
        ranked(&[(0, 0), (2, 1)]),
        ranked(&[(0, 0), (1, 2), (5, 1)]),
        ranked(&[(0, 0), (2, 1), (3, 2)]),
    ];
    let any_number = |_: usize| Some(usize::MAX);
    let no_candidates = |_: usize| Some(0);

    let unweighed = scanner.plan_at(1, &pairs, (0xFF, 0), None);
    let affordable = scanner.plan_at(1, &pairs, (0xFF, 0), Some(&any_number));
    let unaffordable = scanner.plan_at(1, &pairs, (0xFF, 0), Some(&no_candidates));

    assert_eq!(unweighed, None);
    // Telling the pairs apart by their first bytes leaves one candidate, by their
    // second bytes two.
    let (candidates, plan) = affordable.expect("the pairs differ in their first bytes");
    assert_eq!((candidates, plan.anchors), (1, anchors(&[0, 2, 0])));
    assert_eq!(unaffordable, None);
}

// Only aarch64 scans sets larger than the ones scanned unweighed.
#[cfg(target_arch = "aarch64")]
#[test]
fn large_sets_are_scanned_only_where_the_scan_outpaces_packed_search() {
    // Tokens on every other line, or on three lines the sample misses.
    let dense = lines_with_tokens(&TOKENS, 0, 2, usize::MAX);
    let sparse = lines_with_tokens(&TOKENS, 10, 1, 3);
    // Six pairs pay without a match in the sample, and eight exact ones do not.
    let six = scanner(&TOKENS[..6], CaseMode::Sensitive);
    let eight = scanner(&TOKENS, CaseMode::Sensitive);

    let dense_plan = eight
        .plan(&dense, LATER_PIECE)
        .expect("the sample holds tokens");
    let count = finished(&eight, &dense_plan, &TOKENS, &dense);
    let sparse_eight = eight.plan(&sparse, LATER_PIECE);
    let sparse_six = six.plan(&sparse, LATER_PIECE);
    let short_whole_six = six.plan(&sparse, WHOLE);
    let long_dense = dense.repeat(4);
    let long_whole = eight.plan(&long_dense, WHOLE);
    let shared_whole = eight.plan(
        &long_dense,
        Extent::Whole {
            threads: NonZeroUsize::new(4).expect("four is not zero"),
        },
    );

    assert_eq!(dense_plan.pair_count, TOKENS.len());
    assert_eq!(count, 2_000);
    assert_eq!(sparse_eight, None);
    // Six exact pairs pay only with no candidates and no packed-search stops.
    assert!(sparse_six.is_some());
    // Too short to weigh, whole or as one of four threads' shares.
    assert!(sparse.len() < MIN_WEIGHED_SHARE_LEN);
    assert_eq!(short_whole_six, None);
    assert!(long_dense.len() >= MIN_WEIGHED_SHARE_LEN);
    assert!(long_whole.is_some());
    assert!(long_dense.len() / 4 < MIN_WEIGHED_SHARE_LEN);
    assert_eq!(shared_whole, None);
}

#[test]
fn scans_are_weighed_where_one_thread_searches_enough_of_the_input() {
    let two_threads = Extent::Whole {
        threads: NonZeroUsize::new(2).expect("two is not zero"),
    };
    let first_piece = Extent::Piece { preceded_by: 0 };
    let last_short_piece = Extent::Piece {
        preceded_by: MIN_WEIGHED_SHARE_LEN - 1,
    };
    let endless = Extent::Piece {
        preceded_by: usize::MAX,
    };

    let weighed = [
        WHOLE.weighs_scans(MIN_WEIGHED_SHARE_LEN),
        two_threads.weighs_scans(2 * MIN_WEIGHED_SHARE_LEN),
        last_short_piece.weighs_scans(1),
        endless.weighs_scans(1),
    ];
    let unweighed = [
        WHOLE.weighs_scans(MIN_WEIGHED_SHARE_LEN - 1),
        two_threads.weighs_scans(2 * MIN_WEIGHED_SHARE_LEN - 1),
        first_piece.weighs_scans(MIN_WEIGHED_SHARE_LEN - 1),
    ];

    // Nothing is weighed on a target the estimates are not for.
    assert_eq!(weighed, [WEIGHS_SCANS; 4]);
    assert_eq!(unweighed, [false; 3]);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn large_sets_are_not_scanned_for_pairs_too_frequent_to_afford() {
    // Each line holds one pattern's pairs but no pattern: few enough candidates for an
    // unweighed set, but six pairs without packed-search stops need zero.
    let patterns = ["abc", "def", "ghi", "jkl", "mno", "pqr"];
    let mut haystack = Vec::with_capacity(12_000 * 120);
    for line in 0..12_000_usize {
        // A slice pattern matches only that exact length; `else` covers the rest.
        let [first, second, third] = *patterns[line % patterns.len()].as_bytes() else {
            unreachable!("the patterns are three bytes long");
        };
        let mut text = [b'.'; 120];
        text[40..49]
            .copy_from_slice(&[first, second, b'.', second, third, b'.', first, b'.', third]);
        text[119] = b'\n';
        haystack.extend_from_slice(&text);
    }
    let six = scanner(&patterns, CaseMode::Sensitive);
    let four = scanner(&patterns[..4], CaseMode::Sensitive);

    let six_plan = six.plan(&haystack, WHOLE);
    let four_plan = four.plan(&haystack, WHOLE);

    assert!(haystack.len() >= MIN_WEIGHED_SHARE_LEN);
    assert_eq!(six_plan, None);
    assert!(four_plan.is_some());
}

/// `pattern` with every other letter case-flipped when `insensitive`.
fn occurrence(pattern: &str, insensitive: bool) -> Vec<u8> {
    pattern
        .bytes()
        .enumerate()
        .map(|(index, byte)| {
            if !insensitive || index % 2 == 0 {
                byte
            } else if byte.is_ascii_lowercase() {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            }
        })
        .collect()
}

#[test]
fn occurrences_are_found_at_every_offset_of_short_haystacks() {
    // Plans are made for one haystack and used on pieces of any length.
    let sampled = log(6_000);
    let long = format!("L0ng-pattern:{}", "0123456789".repeat(7));
    let pattern_sets: [&[&str]; 3] = [
        &["ERROR", "status=500"],
        // Two bytes allow only adjacent pairs; the other pattern is longer than a block.
        &["Zq", &long],
        // Punctuation that the sampled haystack does not hold.
        &["A_B[warn]^"],
    ];
    for patterns in pattern_sets {
        for case_mode in CASE_MODES {
            let scanner = scanner(patterns, case_mode);
            let plan = scanner.plan(&sampled, WHOLE).expect("the pairs are rare");
            let [first, last] = [patterns[0], patterns[patterns.len() - 1]]
                .map(|pattern| occurrence(pattern, scanner.insensitive));
            // The occurrence ends the haystack, a line, or neither; then follow partial
            // or further occurrences.
            let rests: [Vec<u8>; 4] = [
                Vec::new(),
                b"\n".to_vec(),
                [b"\n......\n", &last[..last.len() - 1], b"\n"].concat(),
                [b"... ", first.as_slice(), b" ...\n...\n", first.as_slice()].concat(),
            ];
            for offset in 0..3 * BLOCK {
                for rest in &rests {
                    let mut haystack = vec![b'.'; offset];
                    haystack.extend_from_slice(&last);
                    haystack.extend_from_slice(rest);

                    let count = finished(&scanner, &plan, patterns, &haystack);

                    assert_eq!(
                        count,
                        reference(patterns, &haystack, scanner.insensitive),
                        "{patterns:?} {case_mode:?} {offset} {}",
                        rest.len()
                    );
                }
            }
            assert_eq!(scanner.count_lines(&plan, b""), (0, 0));
        }
    }
}

#[test]
fn folded_pairs_with_a_byte_that_folding_changes_find_every_occurrence() {
    let sampled = log(6_000);
    // A two-byte pattern has one pair; `_` has bit 5 clear, and folding sets it.
    let patterns = ["_a"];
    let scanner = scanner(&patterns, CaseMode::AsciiInsensitive);
    let plan = scanner.plan(&sampled, WHOLE).expect("the pair is rare");
    // DEL is `_` with bit 5 set: the kernel flags it and verification rejects it.
    let haystack = b"...\x7fA...\n._A.\n.. _a\n\x7fa\n".repeat(40);

    let count = finished(&scanner, &plan, &patterns, &haystack);

    assert!(plan.fold);
    assert_eq!(plan.pairs[..plan.pair_count], [[b'_' | 0x20, b'a']]);
    assert_eq!(count, 80);
    assert_eq!(count, reference(&patterns, &haystack, true));
}

#[test]
fn scans_leave_only_a_few_bytes_for_every_tail_length() {
    let body = log(3_000);
    let patterns = ["error", "status=500"];
    for case_mode in CASE_MODES {
        let scanner = scanner(&patterns, case_mode);
        let plan = scanner.plan(&body, WHOLE).expect("the pairs are rare");
        for tail in 0..2 * BLOCK {
            for end in ["", "\n"] {
                let mut haystack = body.clone();
                haystack.extend_from_slice(&b"tail error status=500 no end!!"[..tail.min(30)]);
                haystack.extend(std::iter::repeat_n(b'y', tail));
                haystack.extend_from_slice(end.as_bytes());

                let (count, unscanned) = scanner.count_lines(&plan, &haystack);

                let rest = reference(&patterns, &haystack[unscanned..], scanner.insensitive);
                assert_eq!(
                    count + rest,
                    reference(&patterns, &haystack, scanner.insensitive),
                    "{case_mode:?} {tail} {end:?}"
                );
                assert!(
                    haystack.len() - unscanned < BLOCK + MAX_DISTANCE + HEAD_LEN + scanner.longest
                );
            }
        }
    }
}

#[test]
fn unterminated_inputs_leave_only_a_tail() {
    let sampled = log(6_000);
    let scanner = scanner(&["ERROR"], CaseMode::Sensitive);
    let plan = scanner.plan(&sampled, WHOLE).expect("the pair is rare");
    let unmatched = vec![b'y'; 200_000];
    let mut matched = unmatched.clone();
    matched[100_000..100_005].copy_from_slice(b"ERROR");

    let (unmatched_count, unmatched_rest) = scanner.count_lines(&plan, &unmatched);
    let matched_result = scanner.count_lines(&plan, &matched);

    // With no line feed, only the tail and a possible straddling start are left.
    assert_eq!(unmatched_count, 0);
    assert!(unmatched.len() - unmatched_rest < BLOCK + MAX_DISTANCE + HEAD_LEN + scanner.longest);
    assert_eq!(matched_result, (1, matched.len()));
}

#[test]
fn plans_pick_pairs_rare_in_the_haystack_and_fold_only_letters() {
    let haystack = log(6_000);

    let sparse = scanner(&["request_id=1999"], CaseMode::Sensitive)
        .plan(&haystack, WHOLE)
        .expect("a rare pair exists");
    let digits = scanner(&["REQUEST_ID=1999"], CaseMode::AsciiInsensitive)
        .plan(&haystack, WHOLE)
        .expect("a rare pair exists");
    let letters = scanner(&["error"], CaseMode::AsciiInsensitive)
        .plan(&haystack, WHOLE)
        .expect("a rare pair exists");
    let shared = scanner(&["WARN", "WARN"], CaseMode::Sensitive)
        .plan(&haystack, WHOLE)
        .expect("a rare pair exists");

    // `request_id=` is in every line, so the pair lies in the digits after it.
    assert!(sparse.anchors[0] >= "request_id=".len());
    assert!(!digits.fold);
    assert!(letters.fold);
    assert_eq!(shared.pair_count, 1);
}

#[test]
fn declines_short_haystacks_frequent_pairs_and_unscannable_sets() {
    let haystack = log(6_000);
    let dna = b"ACGT".repeat(64 * 1024);

    assert!(
        scanner(&["ERROR"], CaseMode::Sensitive)
            .plan(&haystack[..MIN_HAYSTACK_LEN - 1], WHOLE)
            .is_none()
    );
    assert!(
        scanner(&["ERROR"], CaseMode::Sensitive)
            .plan(&haystack[..MIN_HAYSTACK_LEN], WHOLE)
            .is_some()
    );
    assert!(
        scanner(&["ACGTAC"], CaseMode::Sensitive)
            .plan(&dna, WHOLE)
            .is_none()
    );
    assert!(PairScanner::new(&owned(&["a"]), case_config(CaseMode::Sensitive), None).is_none());
    assert!(
        PairScanner::new(&owned(&["ab", "c"]), case_config(CaseMode::Sensitive), None).is_none()
    );
    let too_many = vec![b"ab".to_vec(); MAX_PATTERNS + 1];
    assert!(PairScanner::new(&too_many, case_config(CaseMode::Sensitive), None).is_none());
    assert!(PairScanner::new(&[], case_config(CaseMode::Sensitive), None).is_none());
}

#[test]
fn plans_decline_rivals_that_stop_seldom_or_less_often_than_the_pairs() {
    // Every 200 bytes hold the pattern twice and `rivals` once each.
    let haystack = |rivals: &str| {
        let filler = ".".repeat(200 - 2 * "warn ".len() - rivals.len() - 1);
        format!("warn WARN {rivals}{filler}\n")
            .repeat(1_000)
            .into_bytes()
    };
    let rivaled = |rival: u8| {
        PairScanner::new(
            &owned(&["warn"]),
            case_config(CaseMode::AsciiInsensitive),
            Some(rival),
        )
        .expect("one pattern is scannable")
    };

    // `w` stops as often as the scan has candidates.
    assert!(rivaled(b'w').plan(&haystack(""), WHOLE).is_some());
    assert!(rivaled(b'q').plan(&haystack("qqq"), WHOLE).is_some());
    // One `q` per two occurrences: the pairs are more frequent.
    assert!(rivaled(b'q').plan(&haystack("q"), WHOLE).is_none());
    // No `q` at all: its search never stops.
    assert!(rivaled(b'q').plan(&haystack(""), WHOLE).is_none());
    // `z` is as frequent as the pattern it is in, and both are seldom.
    let seldom = format!("{}zap\n", ".".repeat(2_000)).repeat(100);
    let zap = |rival| {
        PairScanner::new(
            &owned(&["zap"]),
            case_config(CaseMode::AsciiInsensitive),
            rival,
        )
    };
    assert!(
        zap(Some(b'z'))
            .expect("one pattern is scannable")
            .plan(seldom.as_bytes(), WHOLE)
            .is_none()
    );
    assert!(
        zap(None)
            .expect("one pattern is scannable")
            .plan(seldom.as_bytes(), WHOLE)
            .is_some()
    );
    assert!(
        scanner(&["warn"], CaseMode::AsciiInsensitive)
            .plan(&haystack(""), WHOLE)
            .is_some()
    );
}

#[test]
fn scans_stop_early_when_the_sample_misjudged_the_haystack() {
    // Only unsampled text holds `ab`, every two bytes, so the sample sees no pair.
    let mut decoys = b"ab".repeat(MIN_HAYSTACK_LEN / 2);
    for newline in (99..decoys.len()).step_by(100) {
        decoys[newline] = b'\n';
    }
    for start in window_starts(decoys.len(), window_count(decoys.len())) {
        decoys[start..][..SAMPLE_WINDOW_LEN].fill(b'.');
    }
    let pattern = "ab-ab+ab:c";
    let scanner = scanner(&[pattern], CaseMode::Sensitive);
    let plan = scanner
        .plan(&decoys, WHOLE)
        .expect("the sample holds no pair");
    let (_, stalled) = scanner.count_lines(&plan, &decoys);
    // The scan gives up after the first batch of blocks.
    assert!(stalled <= BATCH * BLOCK);
    // Occurrences before, far after, and at every offset around where it gives up.
    for offset in stalled - BLOCK..stalled + 4 * BLOCK {
        let mut haystack = decoys.clone();
        for start in [10, offset, 90_000] {
            haystack[start..start + pattern.len()].copy_from_slice(pattern.as_bytes());
        }

        let (count, unscanned) = scanner.count_lines(&plan, &haystack);

        let rest = reference(&[pattern], &haystack[unscanned..], false);
        assert_eq!(
            count + rest,
            reference(&[pattern], &haystack, false),
            "{offset}"
        );
        assert!(count >= 1);
        assert!(unscanned < 2 * BATCH * BLOCK);
    }
}

#[test]
fn kernel_flags_exactly_the_positions_holding_a_pair() {
    let pairs = [*b"er", *b"=5"];
    for position in 0..BLOCK {
        let mut keys = [b'.'; BLOCK];
        let mut values = [b'.'; BLOCK];
        // Uppercase letters match the lowercase pair only when folding.
        (keys[position], values[position]) = (b'E', b'R');
        // Half a pair, at another position, is no pair.
        keys[(position + 1) % BLOCK] = b'=';

        let (folded, any_folded) = pair_flags::<2, true>(&pairs, 0, &keys, &values);
        let (exact, any_exact) = pair_flags::<2, false>(&pairs, 0xFF, &keys, &values);
        values[(position + 1) % BLOCK] = b'5';
        let (both, _) = pair_flags::<2, false>(&pairs, 0xFF, &keys, &values);
        let (single, _) = pair_flags::<1, false>(&[*b"=5"], 0xFF, &keys, &values);

        assert_eq!(flag_bits(&folded), 1 << position);
        assert_ne!(any_folded, 0);
        assert_eq!(flag_bits(&exact), 0);
        assert_eq!(any_exact, 0);
        assert_eq!(flag_bits(&both), 1 << ((position + 1) % BLOCK));
        assert_eq!(flag_bits(&single), flag_bits(&both));
    }
}

#[test]
fn kernel_selects_each_value_by_its_key_and_flags_absent_values() {
    let pairs = [*b"ab", *b"cd", *b"ef", *b"gh"];
    let mut keys = [b'.'; BLOCK];
    let mut values = [b'.'; BLOCK];
    // Every pair once, then every key with another pair's value.
    for (index, pair) in pairs.iter().enumerate() {
        (keys[index], values[index]) = (pair[0], pair[1]);
        (keys[8 + index], values[8 + index]) = (pair[0], pairs[(index + 1) % 4][1]);
    }
    // The absent byte as a value: flagged without a key, not with the key of a pair.
    values[20] = b'~';
    (keys[21], values[21]) = (b'a', b'~');

    let (four, _) = pair_flags::<4, false>(&pairs, b'~', &keys, &values);
    let (folded, _) = pair_flags::<4, true>(&pairs, 0, &keys, &values);
    let two: &[[u8; 2]; 2] = pairs.first_chunk().expect("four pairs hold two");
    let (first_two, _) = pair_flags::<2, false>(two, b'~', &keys, &values);

    assert_eq!(flag_bits(&four), 0b1111 | 1 << 20);
    // No folded byte equals zero, so a folding kernel flags pairs only.
    assert_eq!(flag_bits(&folded), 0b1111);
    assert_eq!(flag_bits(&first_two), 0b0011 | 1 << 20);
}

#[test]
fn plans_key_pairs_on_the_stream_where_their_bytes_differ() {
    let anchors = [0; MAX_PATTERNS];

    let first = PairPlan::new(
        &scanner(&["ab", "cb"], CaseMode::Sensitive),
        1,
        anchors,
        0xFF,
    )
    .expect("the first bytes differ");
    let second = PairPlan::new(
        &scanner(&["ab", "ac"], CaseMode::Sensitive),
        1,
        anchors,
        0xFF,
    )
    .expect("the second bytes differ");
    let neither = PairPlan::new(
        &scanner(&["ab", "ac", "db"], CaseMode::Sensitive),
        1,
        anchors,
        0xFF,
    );
    let folded = PairPlan::new(
        &scanner(&["Ab", "a_"], CaseMode::AsciiInsensitive),
        1,
        anchors,
        0xFF,
    )
    .expect("the second bytes differ");

    assert!(!first.keyed_on_second);
    assert_eq!(first.pairs[..first.pair_count], [*b"ab", *b"cb"]);
    assert_eq!(first.key_offset(), 0);
    // Pairs are stored key first.
    assert!(second.keyed_on_second);
    assert_eq!(second.pairs[..second.pair_count], [*b"ba", *b"ca"]);
    assert_eq!(second.key_offset(), 1);
    assert_eq!(second.owners[usize::from(b'b')], 0b01);
    assert_eq!(second.owners[usize::from(b'c')], 0b10);
    assert_eq!(second.owners[usize::from(b'a')], 0);
    assert!(second.flags_absent());
    assert_eq!(second.absent, 0xFF);
    assert!(neither.is_none());
    // Folding sets bit 5 of keys too, and needs no absent byte from the haystack.
    assert_eq!(folded.owners[usize::from(b'_' | 0x20)], 0b10);
    assert_eq!(folded.absent, 0);
    assert!(!folded.flags_absent());
}

#[test]
fn scans_keyed_on_either_stream_count_the_same_lines() {
    // The pairs share their first byte in one set and second in the other.
    for patterns in [["k=5", "k=4"], ["5=k", "4=k"]] {
        for case_mode in CASE_MODES {
            let mut haystack = Vec::new();
            for index in 0..4_000 {
                let field = ["k=5", "K=4", "k=3", "5=K", "4=k", "k=5 4=k"][index % 6];
                haystack.extend_from_slice(format!("{field} {index} ").as_bytes());
                // `0xFF` is the unfolded plans' absent byte: rejected, but rarely enough.
                let end: &[u8] = if index % 40 == 0 { b"\xFF\n" } else { b"..\n" };
                haystack.extend_from_slice(end);
            }
            let scanner = scanner(&patterns, case_mode);
            let plan = PairPlan::new(&scanner, 2, [0; MAX_PATTERNS], 0xFF)
                .expect("one stream tells the pairs apart");

            let (count, unscanned) = scanner.count_lines(&plan, &haystack);

            let rest = reference(&patterns, &haystack[unscanned..], scanner.insensitive);
            assert_eq!(plan.keyed_on_second, patterns[0].starts_with('k'));
            assert_eq!(plan.fold, scanner.insensitive);
            assert!(haystack.len() - unscanned < 2 * BLOCK, "the scan finished");
            assert_eq!(
                count + rest,
                reference(&patterns, &haystack, scanner.insensitive),
                "{patterns:?} {case_mode:?}"
            );
        }
    }
}

#[test]
fn lone_pairs_are_keyed_on_their_rarer_byte_and_tested_alone_when_it_is_seldom() {
    let anchors = [0; MAX_PATTERNS];
    let lone = |patterns: &[&str], case_mode: CaseMode| {
        PairPlan::new(&scanner(patterns, case_mode), 1, anchors, 0xFF)
            .expect("the pairs have distinct keys")
    };
    let mut byte_counts = [0_u16; 256];
    byte_counts[usize::from(b'a')] = 9;
    byte_counts[usize::from(b'b')] = 4;
    // Folding flags `@` where the plan holds a backtick, so both count for that byte.
    byte_counts[usize::from(b'@')] = 3;
    byte_counts[usize::from(b'`')] = 2;
    let mut rekeyed = lone(&["ab", "abc"], CaseMode::Sensitive);
    let mut kept = lone(&["ba"], CaseMode::Sensitive);
    let mut frequent = lone(&["ab"], CaseMode::Sensitive);
    let mut folded = lone(&["b@"], CaseMode::AsciiInsensitive);
    let mut several = lone(&["ab", "cb"], CaseMode::Sensitive);
    let untouched = several.clone();

    rekeyed.key_on_rarer_byte(&byte_counts, 4 * KEY_ALONE_SPACING);
    kept.key_on_rarer_byte(&byte_counts, 4 * KEY_ALONE_SPACING);
    frequent.key_on_rarer_byte(&byte_counts, 4 * KEY_ALONE_SPACING - 1);
    folded.key_on_rarer_byte(&byte_counts, 4 * KEY_ALONE_SPACING);
    several.key_on_rarer_byte(&byte_counts, usize::MAX);

    assert!(rekeyed.keyed_on_second && rekeyed.key_alone);
    assert_eq!(rekeyed.pairs[0], *b"ba");
    assert_eq!(rekeyed.key_offset(), 1);
    // Both patterns own the pair, under its new key only.
    assert_eq!(rekeyed.owners[usize::from(b'b')], 0b11);
    assert_eq!(rekeyed.owners[usize::from(b'a')], 0);
    assert!(!kept.keyed_on_second && kept.key_alone);
    assert_eq!(kept.pairs[0], *b"ba");
    assert!(frequent.keyed_on_second && !frequent.key_alone);
    // The letter occurs four times and the other byte five, in its two spellings.
    assert!(!folded.keyed_on_second && folded.key_alone);
    assert_eq!(folded.pairs[0], *b"b`");
    assert_eq!(several, untouched);
}

#[test]
fn key_flags_tell_the_blocks_that_hold_the_key_in_either_case_when_folding() {
    for position in 0..BLOCK {
        let mut block = [b'.'; BLOCK];
        block[position] = b'K';

        assert_ne!(key_flag::<false>(b'K', &block), 0, "{position}");
        assert_eq!(key_flag::<false>(b'k', &block), 0, "{position}");
        assert_ne!(key_flag::<true>(b'k', &block), 0, "{position}");
    }
    assert_eq!(key_flag::<true>(b'k', &[b'.'; BLOCK]), 0);
}

#[test]
fn scans_testing_the_key_alone_count_the_lines_a_scan_of_both_streams_counts() {
    let haystack = log(6_000);
    for patterns in [["ERROR"], ["Error"], ["id=59"], ["absent"]] {
        for case_mode in CASE_MODES {
            let scanner = scanner(&patterns, case_mode);
            let both = PairPlan::new(&scanner, 1, [1; MAX_PATTERNS], 0xFF)
                .expect("one pair has a distinct key");
            for keyed_on_second in [false, true] {
                let mut alone = both.clone();
                // With all-zero counts the key stays and is tested alone.
                alone.key_on_rarer_byte(&[0; 256], 0);
                if keyed_on_second {
                    alone.pairs[0].reverse();
                    alone.keyed_on_second = true;
                    alone.owners = [0; 256];
                    alone.owners[usize::from(alone.pairs[0][0])] = 1;
                }

                let counted = scanner.count_lines(&alone, &haystack);

                assert!(alone.key_alone);
                assert_eq!(
                    counted,
                    scanner.count_lines(&both, &haystack),
                    "{patterns:?} {case_mode:?} {keyed_on_second}"
                );
                assert_eq!(
                    finished(&scanner, &alone, &patterns, &haystack),
                    reference(&patterns, &haystack, scanner.insensitive)
                );
            }
        }
    }
}

/// What [`for_each_mark`] finds of `marks` in `bytes`.
fn found_marks<const MARKS: usize>(bytes: &[u8], marks: &[[u8; 2]; MARKS]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for_each_mark::<MARKS, false>(bytes, marks, |position, mark| {
        found.push((position, mark));
    });
    found
}

#[test]
fn folding_scans_visit_marks_in_either_case_and_nothing_else() {
    let marks = [*b"wo", *b"[1", *b" x"];
    // A byte that differs from a mark's only in bit 5 is the mark only if it is a letter.
    let text = b"WO wo Wo {1 [1 [\x11 \x00x  X\x00X wO \xDF\xFF".repeat(40);
    let batch = MARK_BATCH * BLOCK;
    assert!(text.len() > batch + 2);

    for len in [0, 1, 2, BLOCK, batch + 1, text.len()] {
        let bytes = &text[..len];
        let expected = (0..len.saturating_sub(1))
            .filter_map(|position| {
                let pair = [bytes[position], bytes[position + 1]].map(fold_ascii);
                let mark = marks.iter().position(|mark| *mark == pair)?;
                Some((position, mark))
            })
            .collect::<Vec<_>>();
        let mut found = Vec::new();

        for_each_mark::<3, true>(bytes, &marks, |position, mark| {
            found.push((position, mark));
        });

        assert_eq!(found, expected, "{len}");
    }
}

#[test]
fn marks_whose_first_bytes_differ_only_in_bit_5_are_not_scanned_folding() {
    let scanned = [
        marks_differ_folded(&[*b"ab", *b"bc", *b"@a"]),
        marks_differ_folded(&[*b"@a", *b"`a"]),
        marks_differ_folded(&[*b"\na", *b"zz", *b"*a"]),
        marks_differ_folded::<0>(&[]),
    ];

    assert_eq!(scanned, [true, false, false, true]);
}

#[test]
fn every_mark_is_visited_with_its_index_in_inputs_shorter_and_longer_than_a_batch() {
    let marks = [*b"[[", *b"\"\"", *b"''"];
    // `0xFF` twice is what the compare flags without it being a mark.
    let text = b"[[a]] x = \"\" [b] 'c' '' \xFF\xFF k = [[1], [2]] \"\"\" ".repeat(70);
    let batch = MARK_BATCH * BLOCK;
    assert!(text.len() > 3 * batch);
    let around_batches = [batch, batch + 1, batch + 2, 2 * batch + 1, text.len()];

    for len in (0..3 * BLOCK).chain(around_batches) {
        let bytes = &text[..len];
        let expected = (0..len.saturating_sub(1))
            .filter_map(|position| {
                let pair = [bytes[position], bytes[position + 1]];
                let mark = marks.iter().position(|mark| *mark == pair)?;
                Some((position, mark))
            })
            .collect::<Vec<_>>();

        assert_eq!(found_marks(bytes, &marks), expected, "{len}");
    }
}

#[test]
fn marks_that_hold_the_byte_of_no_mark_are_found_and_nothing_else_is() {
    // The compare takes `0xFF` for what follows a byte that starts no mark.
    let marks = [[0xFF, 0xFF], [b'a', 0xFF], *b"bc"];
    let bytes = b"a\xFF\xFF\xFFb\xFFbc\xFFa x\xFF".repeat(40);
    let expected = (0..bytes.len() - 1)
        .filter_map(|position| {
            let pair = [bytes[position], bytes[position + 1]];
            let mark = marks.iter().position(|mark| *mark == pair)?;
            Some((position, mark))
        })
        .collect::<Vec<_>>();

    let found = found_marks(&bytes, &marks);

    assert!(expected.len() > 100);
    assert_eq!(found, expected);
}

#[test]
fn marks_are_visited_where_every_position_holds_one_and_not_in_the_padding() {
    let batch = MARK_BATCH * BLOCK;
    let brackets = b"[".repeat(2 * batch + 7);
    // A mark of zeros is what the padding after the last positions holds.
    let zeros = [0_u8; BLOCK + 9];

    let all = found_marks(&brackets, &[*b"[["]);
    let unpadded = found_marks(&zeros, &[[0, 0], *b"[["]);

    assert!(
        all.into_iter()
            .eq((0..brackets.len() - 1).map(|position| (position, 0)))
    );
    assert!(
        unpadded
            .into_iter()
            .eq((0..zeros.len() - 1).map(|position| (position, 0)))
    );
}

#[test]
fn adjacent_blocks_pair_every_position_with_the_byte_after_it() {
    let text = (0..=u8::MAX)
        .cycle()
        .skip(1)
        .take(3 * BLOCK)
        .collect::<Vec<_>>();
    for len in [
        0,
        1,
        2,
        BLOCK,
        BLOCK + 1,
        BLOCK + 2,
        2 * BLOCK + 1,
        3 * BLOCK,
    ] {
        let bytes = &text[..len];

        let (firsts, seconds, [last_firsts, last_seconds]) = adjacent_blocks(bytes);

        let positions = len.saturating_sub(1);
        let pairs = firsts
            .iter()
            .chain([&last_firsts])
            .flatten()
            .zip(seconds.iter().chain([&last_seconds]).flatten())
            .collect::<Vec<_>>();
        assert_eq!(pairs.len(), (positions / BLOCK + 1) * BLOCK, "{len}");
        for (position, (&first, &second)) in pairs.into_iter().enumerate() {
            let expected = if position < positions {
                (bytes[position], bytes[position + 1])
            } else {
                (0, 0)
            };
            assert_eq!((first, second), expected, "{len} at {position}");
        }
    }
}

#[test]
fn flagged_positions_are_those_of_the_set_flags_in_ascending_order() {
    let mut flags = [0_u8; BLOCK];
    for position in [0, 7, 8, 33, BLOCK - 1] {
        flags[position] = u8::MAX;
    }

    let positions = flagged_positions(&flags).collect::<Vec<_>>();
    let none = flagged_positions(&[0; BLOCK]).count();

    assert_eq!(positions, [0, 7, 8, 33, BLOCK - 1]);
    assert_eq!(none, 0);
}

#[test]
fn samples_count_the_positions_that_hold_a_pair() {
    // `ab` twice, `bc` once, `ca` never; 16-byte lines make every window alike.
    let text = b"ab abc cb ba ac\n".repeat(4_096);

    let sample = PairSample::new(&text, false).expect("the text is longer than a window");
    let folded = PairSample::new(&text.to_ascii_uppercase(), true).expect("as long");
    let [ab, bc, ca] = [*b"ab", *b"bc", *b"ca"].map(|pair| sample.occurrences(pair));
    let shortest = PairSample::new(&text[..SAMPLE_WINDOW_LEN], false);
    let too_short = PairSample::new(&text[..SAMPLE_WINDOW_LEN - 1], false);
    let shared = PairSample::for_share(&text.repeat(128), false, 2 * 1024 * 1024)
        .expect("the text is longer than a window");

    // Windows follow the 2 MiB share, not the 8 MiB haystack.
    assert_eq!(shared.windows().len(), 8);
    assert_eq!(ab, 2 * bc);
    assert!(bc > 0 && bc <= sample.counted_positions());
    assert_eq!(ca, 0);
    assert_eq!(folded, sample);
    assert!(shortest.is_some());
    assert_eq!(too_short, None);
}

#[test]
fn patterns_sharing_a_pair_are_each_tried_at_their_own_offset() {
    // `ER` at different offsets of two patterns, and the same pattern twice.
    for (patterns, anchors) in [
        (["an ERROR", "ERROR code"], anchors(&[3])),
        (["ERROR", "ERROR"], anchors(&[])),
    ] {
        let scanner = scanner(&patterns, CaseMode::Sensitive);
        let plan = PairPlan::new(&scanner, 1, anchors, 0xFF).expect("one pair has one key");
        let haystack = b"an ERROR\nERROR code\nERROR\nan ERROR code\nERRO code\n".repeat(2_000);

        let count = finished(&scanner, &plan, &patterns, &haystack);

        assert_eq!(plan.pairs[..plan.pair_count], [*b"ER"]);
        assert_eq!(plan.owners[usize::from(b'E')], 0b11);
        assert!(!plan.flags_absent());
        assert_eq!(
            count,
            reference(&patterns, &haystack, false),
            "{patterns:?}"
        );
    }
}

#[test]
fn lines_longer_than_a_batch_count_once_wherever_their_occurrences_lie() {
    let sampled = log(6_000);
    // A head of exactly sixteen bytes, and one byte more than a head.
    for pattern in ["sixteen-byte-pat", "seventeen-byte-pa"] {
        assert!((HEAD_LEN..=HEAD_LEN + 1).contains(&pattern.len()));
        let patterns = [pattern];
        let scanner = scanner(&patterns, CaseMode::Sensitive);
        let plan = scanner.plan(&sampled, WHOLE).expect("the pair is rare");
        let batch = BATCH * BLOCK;
        // Occurrences around every batch boundary of three long lines; the last line
        // ends with a partial one.
        let mut haystack = Vec::new();
        for line in 0..3 {
            for _ in 0..5 {
                haystack.extend_from_slice(pattern.as_bytes());
                haystack.extend(std::iter::repeat_n(b'-', batch / 2 - 2 * pattern.len()));
                haystack.extend_from_slice(pattern.as_bytes());
            }
            haystack.extend_from_slice(if line == 1 { b"\n\n" } else { b"\n" });
        }
        haystack.extend_from_slice(&pattern.as_bytes()[..pattern.len() - 1]);
        haystack.extend(std::iter::repeat_n(b'-', 3 * BLOCK));

        let count = finished(&scanner, &plan, &patterns, &haystack);

        assert_eq!(count, 3, "{pattern}");
    }
}

#[test]
fn literals_compare_their_head_masked_and_their_tail_in_full() {
    let long = "Request-Id: 0123456789abcdef";
    let exact = Literal::new(b"A_b", CaseMode::Sensitive);
    let folded = Literal::new(b"A_b", CaseMode::AsciiInsensitive);
    let tailed = Literal::new(long.as_bytes(), CaseMode::AsciiInsensitive);
    let cased = Literal::new(long.to_uppercase().as_bytes(), CaseMode::Sensitive);
    let uncased = Literal::new(long.as_bytes(), CaseMode::Sensitive);
    let haystack = format!("a_B A_b a\x7fb {} {}!", long.to_uppercase(), &long[..27]);
    let haystack = haystack.as_bytes();
    let occurrences = |literal: &Literal, insensitive: bool| -> Vec<usize> {
        (0..haystack.len() + 2)
            .filter(|&start| literal.occurs_at(haystack, start, insensitive))
            .collect()
    };

    // Only letters match in either case: DEL differs from `_` in bit 5 alone.
    assert_eq!(occurrences(&exact, false), [4]);
    assert_eq!(occurrences(&folded, true), [0, 4]);
    // A tail differing in its last byte, or cut off by the haystack's end.
    assert_eq!(occurrences(&tailed, true), [12]);
    assert!(!tailed.occurs_at(&haystack[..haystack.len() - 1], 41, true));
    assert_eq!(occurrences(&cased, false), [12]);
    assert!(occurrences(&uncased, false).is_empty());
    assert!(!exact.occurs_at(haystack, usize::MAX, false));
    // A head needs all its bytes in the haystack.
    assert!(!exact.occurs_at(b"A_b", 0, false));
}

#[test]
fn flagged_ranges_span_the_first_and_the_last_flag() {
    for (first, last) in [(0, 0), (0, 63), (5, 5), (17, 40), (63, 63)] {
        let mut flags = [0_u8; BLOCK];
        flags[first] = u8::MAX;
        flags[last] = u8::MAX;

        let range = flagged_range(&flags);

        assert_eq!(range, (first as u8, last as u8));
    }
}

#[test]
fn line_tests_find_a_line_feed_at_every_offset_of_ranges_of_every_length() {
    // A line feed at every offset, alone or with another half a block later, and none
    // (an offset past the end sets nothing).
    let length = 4 * BLOCK;
    let lengths = [0, 1, BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK - 1, 2 * BLOCK];
    for feeds in (0..=length).flat_map(|feed| [[feed, length], [feed, feed + BLOCK / 2]]) {
        let mut haystack = vec![b'.'; length];
        for feed in feeds {
            if let Some(byte) = haystack.get_mut(feed) {
                *byte = b'\n';
            }
        }
        // Ranges within the first block, across blocks, and ending the haystack.
        for from in [0, 1, BLOCK - 1, BLOCK, length - BLOCK, length - 1] {
            for len in lengths
                .into_iter()
                .chain([2 * BLOCK + 1, 3 * BLOCK, length])
            {
                let to = (from + len).min(length);
                let expected = feeds.iter().any(|feed| (from..to).contains(feed));

                let ends = line_ends_between(&haystack, from, to);

                assert_eq!(ends, expected, "{feeds:?} {from} {to}");
            }
        }
    }
}

#[test]
fn sampled_windows_spread_from_the_start_to_the_end_of_a_haystack() {
    // A haystack's length, how much of it one thread searches, and its windows.
    for (len, share, count) in [
        (MIN_HAYSTACK_LEN, MIN_HAYSTACK_LEN, MIN_SAMPLE_WINDOWS),
        (1024 * 1024 + 1, 1024 * 1024 + 1, MIN_SAMPLE_WINDOWS),
        (2 * 1024 * 1024, 2 * 1024 * 1024, 8),
        (64 * 1024 * 1024, 64 * 1024 * 1024, MAX_SAMPLE_WINDOWS),
        (64 * 1024 * 1024, 2 * 1024 * 1024, 8),
    ] {
        let starts: Vec<usize> = window_starts(len, window_count(share)).collect();

        assert_eq!(starts.len(), count, "{len}");
        assert_eq!(starts[0], 0);
        assert!(starts.is_sorted());
        let last = starts[count - 1] + SAMPLE_WINDOW_LEN;
        assert!(last <= len && len - last < count, "{len}");
    }
}

#[test]
fn lines_with_several_occurrences_and_blocks_with_several_lines_count_once_each() {
    let sampled = log(6_000);
    let patterns = ["ERROR"];
    let scanner = scanner(&patterns, CaseMode::Sensitive);
    let plan = scanner.plan(&sampled, WHOLE).expect("the pair is rare");
    // Short lines with zero to two matches, then multi-block lines matching at both ends.
    let mut haystack = Vec::new();
    for index in 0..3_000 {
        haystack.extend_from_slice(match index % 4 {
            0 => b"ERROR\n".as_slice(),
            1 => b"ERROR ERROR\n",
            2 => b"error\n",
            _ => b"x ERROR\n",
        });
    }
    for _ in 0..40 {
        haystack.extend_from_slice(b"ERROR ");
        haystack.extend(std::iter::repeat_n(b'-', 5 * BLOCK));
        haystack.extend_from_slice(b" ERROR\n\n");
    }

    let count = finished(&scanner, &plan, &patterns, &haystack);

    assert_eq!(count, 2_250 + 40);
    assert_eq!(count, reference(&patterns, &haystack, false));
}

#[test]
fn sample_counts_cover_the_counted_positions_of_every_window() {
    for distance in 1..=MAX_DISTANCE {
        // The last counted position has its second byte past the counted positions.
        for (start, counted) in [(0, 1), (COUNTED_LEN - 1, 1), (COUNTED_LEN, 0)] {
            let mut window = [b'.'; SAMPLE_WINDOW_LEN];
            window[start] = b'a';
            window[start + distance] = b'b';

            let occurrences = pair_occurrences(&window, distance, *b"ab");

            assert_eq!(occurrences, counted, "{distance} {start}");
        }
    }
    let every_byte: SampleWindow = std::array::from_fn(|index| index as u8);
    let one_byte = [b'a'; SAMPLE_WINDOW_LEN];

    let counts = count_bytes(&[every_byte, one_byte]);

    assert_eq!(counts[usize::from(b'a')], 257);
    assert!(
        counts
            .iter()
            .enumerate()
            .all(|(byte, &count)| count == 1 || byte == usize::from(b'a'))
    );
}

/// Lines of dots, with one line per pattern placed where the sample misses it.
fn dots_with(patterns: &[&str]) -> Vec<u8> {
    let mut haystack = b"................................................\n".repeat(3_000);
    for (line, pattern) in (100..).zip(patterns) {
        haystack[line * 49..][..pattern.len()].copy_from_slice(pattern.as_bytes());
    }
    haystack
}

#[test]
fn colliding_rarest_pairs_have_no_plan_where_scans_are_not_weighed() {
    // The sample holds none of these bytes, so each rarest pair is the pattern's first;
    // at every distance two share a first byte and two a second.
    let patterns = ["abcd", "aefx", "gbcd"];
    let haystack = dots_with(&patterns);
    for case_mode in CASE_MODES {
        let scanner = scanner(&patterns, case_mode);

        let rarest_only = (1..=MAX_DISTANCE)
            .find_map(|distance| PairPlan::new(&scanner, distance, anchors(&[]), 0xFF));
        let short_whole = scanner.plan(&haystack, WHOLE);
        let later_piece = scanner.plan(&haystack, LATER_PIECE);

        assert_eq!(rarest_only, None);
        assert!(haystack.len() < MIN_WEIGHED_SHARE_LEN);
        assert_eq!(short_whole, None);
        // Weighed, the patterns take other pairs.
        assert_eq!(later_piece.is_some(), WEIGHS_SCANS);
    }
}

// Only aarch64 weighs scans, which pairs taken in place of the rarest have to be.
#[cfg(target_arch = "aarch64")]
#[test]
fn patterns_whose_rarest_pairs_collide_at_both_ends_take_their_next_rarest() {
    let patterns = ["abcd", "aefx", "gbcd"];
    let haystack = dots_with(&patterns);
    for case_mode in CASE_MODES {
        let scanner = scanner(&patterns, case_mode);

        let plan = scanner
            .plan(&haystack, LATER_PIECE)
            .expect("later pairs differ");
        let count = finished(&scanner, &plan, &patterns, &haystack);

        // The second pattern gives up `ae` for `ef`, which no other pair begins with.
        assert_eq!((plan.distance, plan.anchors), (1, anchors(&[0, 1, 0])));
        assert_eq!(plan.pair_count, 3);
        assert_eq!(count, 3);
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn pairs_taken_in_place_of_the_rarest_are_scanned_only_where_the_scan_pays() {
    // Every line holds the fallback pairs of two patterns, near the admitted density.
    let mut line = [b'.'; 120];
    line[40..43].copy_from_slice(b"bcd");
    line[80..83].copy_from_slice(b"efx");
    line[119] = b'\n';
    let haystack = line.repeat(3_000);
    for case_mode in CASE_MODES {
        let colliding = scanner(&["abcd", "aefx", "gbcd"], case_mode);
        let rarest = scanner(&["efx"], case_mode);

        let colliding_plan = colliding.plan(&haystack, LATER_PIECE);
        let rarest_plan = rarest
            .plan(&haystack, LATER_PIECE)
            .expect("its pair is its rarest");
        let count = finished(&rarest, &rarest_plan, &["efx"], &haystack);

        assert_eq!(colliding_plan, None);
        assert_eq!(count, 3_000);
    }
}

#[test]
fn the_rarest_plan_is_the_first_of_the_rarest_at_any_distance_within_those_admitted() {
    let dots = [window(&[])];
    // `ab` and `bc` twice each, and no `a` two bytes before a `c`.
    let adjacent = [window(&[(10, "ab"), (20, "ab"), (30, "bc"), (40, "bc")])];
    let plan_of = |pattern: &str, sample: &[SampleWindow], admitted: usize| {
        scanner(&[pattern], CaseMode::Sensitive).rarest_plan(
            sample,
            &count_bytes(sample),
            admitted,
            None,
        )
    };

    let first_of_equals = plan_of("abcd", &dots, 0).expect("the sample holds no pair");
    let rarer_later = plan_of("abc", &adjacent, 2).expect("two candidates are admitted");
    let only_later = plan_of("abc", &adjacent, 1).expect("the wider pair has no candidate");
    let admitted = plan_of("ab", &adjacent, 2);
    let too_frequent = plan_of("ab", &adjacent, 1);

    // Every distance has a plan without a candidate: the search ends at the first.
    assert_eq!(first_of_equals.distance, 1);
    // A later distance wins with fewer candidates, or when the earlier exceeds `admitted`.
    assert_eq!(rarer_later.distance, 2);
    assert_eq!(only_later.distance, 2);
    assert!(admitted.is_some());
    assert_eq!(too_frequent, None);
}

/// Short lines, then a long line with `ERROR` at the end of the first batch and the start
/// of the second, then `ER` every five bytes (enough rejects to end a scan), then 2,000
/// `ERROR` lines. Returns the text and the offset past the long line's line feed.
fn long_line_between_short_ones() -> (Vec<u8>, usize) {
    let batch_len = BATCH * BLOCK;
    let mut haystack = b".......\n".repeat((batch_len - 16) / 8);
    haystack.extend_from_slice(b"ERROR ..........ERROR");
    haystack.resize(2 * batch_len, b'.');
    haystack.extend_from_slice(&b"ER.. ".repeat(3 * batch_len / 5));
    haystack.push(b'\n');
    let long_line_end = haystack.len();
    haystack.extend_from_slice(&b"ERROR..\n".repeat(2_000));
    (haystack, long_line_end)
}

#[test]
fn what_is_left_of_a_line_that_a_later_batch_finds_alone_is_not_compared() {
    let scanner = scanner(&["ERROR"], CaseMode::Sensitive);
    let plan = PairPlan::new(&scanner, 1, anchors(&[0]), 0xFF).expect("one pair has a key");
    let (haystack, _) = long_line_between_short_ones();

    let (count, unscanned) = scanner.count_lines(&plan, &haystack);
    let rest = reference(&["ERROR"], &haystack[unscanned..], false);

    assert_eq!(count + rest, 2_001);
    // The scan went through to the lines after the long one.
    assert!(haystack.len() - unscanned < BLOCK + MAX_DISTANCE + HEAD_LEN + scanner.longest);
}

#[test]
fn a_scan_that_ends_in_the_batch_that_passes_over_a_line_leaves_what_follows_the_line() {
    let scanner = scanner(&["ERROR"], CaseMode::Sensitive);
    let plan = PairPlan::new(&scanner, 1, anchors(&[0]), 0xFF).expect("one pair has a key");
    let (mut haystack, long_line_end) = long_line_between_short_ones();
    // More candidates in the second batch than a scan is allowed to reject by its end.
    let second_batch = BATCH * BLOCK;
    haystack[second_batch + 8..][..800].copy_from_slice(&b"ER..".repeat(200));

    let (count, unscanned) = scanner.count_lines(&plan, &haystack);
    let rest = reference(&["ERROR"], &haystack[unscanned..], false);

    // The long line is counted, and the lines after it are left to the caller.
    assert_eq!((count, unscanned), (1, long_line_end));
    assert_eq!(rest, 2_000);
}

#[test]
fn a_counted_line_ends_after_its_line_feed_or_with_the_haystack() {
    // Lines at 0, 8 and 19, the last without a line feed.
    let haystack = b"one two\nthree four\nfive";

    let none_counted = counted_line_end(haystack, None, 4);
    let within_the_line = counted_line_end(haystack, Some(0), 4);
    let within_the_last_line = counted_line_end(haystack, Some(19), 21);
    let in_a_later_line = counted_line_end(haystack, Some(4), 10);
    let after_the_line_feed = counted_line_end(haystack, Some(7), 8);

    assert_eq!(none_counted, None);
    assert_eq!(within_the_line, Some(8));
    assert_eq!(within_the_last_line, Some(haystack.len()));
    assert_eq!(in_a_later_line, None);
    assert_eq!(after_the_line_feed, None);
}

#[test]
fn rarest_pairs_hold_a_rare_byte_and_occur_least_in_the_sample() {
    let mut window = [b'.'; SAMPLE_WINDOW_LEN];
    window[..2].copy_from_slice(b"az");
    for start in [10, 20, 30] {
        window[start..start + 2].copy_from_slice(b"zb");
    }
    let sample = [window];
    let counts = count_bytes(&sample);

    let rare: [usize; RARE_OFFSETS] = rare_offsets(b"azbc", &counts);
    let short: [usize; RARE_OFFSETS] = rare_offsets(b"zb", &counts);
    let adjacent = PairScanner::ranked_pairs(b"azbc", &rare, 1, &sample, &counts);
    let widest = PairScanner::ranked_pairs(b"azbc", &rare, 3, &sample, &counts);
    let too_wide = PairScanner::ranked_pairs(b"zb", &short, 2, &sample, &counts);

    // `c` never occurs, `a` once, `b` three times and `z` four times.
    assert_eq!(rare, [3, 0, 2]);
    assert_eq!(short, [1, 0, usize::MAX]);
    let pair = |occurrences: usize, byte_occurrences: u16, offset: usize| RankedPair {
        occurrences,
        byte_occurrences,
        offset,
    };
    // `az` occurs once and `zb` three times; `bc` and `a..c` never do.
    assert_eq!(
        adjacent.as_slice(),
        [pair(0, 3, 2), pair(1, 5, 0), pair(3, 7, 1)]
    );
    assert_eq!(widest.as_slice(), [pair(0, 1, 0)]);
    assert!(too_wide.is_empty());
}

#[test]
fn a_page_aligned_value_starts_at_a_page_boundary() {
    let aligned = PageAligned(Tally::default());

    let address = (&raw const aligned).addr();

    assert_eq!(address % 16384, 0);
    assert_eq!(align_of_val(&aligned), 16384);
    assert_eq!(aligned.0, Tally::default());
}
