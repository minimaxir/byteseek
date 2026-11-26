use std::collections::HashSet;

use triple_accel::levenshtein::levenshtein_naive_k;

use super::candidates::{dedup_ends_in_start_order, for_each_window_within_one_edit};
use super::{
    ApproximateCandidate, ApproximateEngine, ApproximatePattern, BitapPrefilter,
    MIN_DENSE_REGION_LEN, MIN_SPARSE_REGION_LEN, MarkPlan, PackedPattern, PrefilterContext,
    RegionLayout, StartRegion, approximate_candidate_cmp, marked_candidates, merge_sorted_runs,
    select_approximate_matches, sort_nearly_sorted, stitch_region_selection,
};
use crate::exact::PairSample;
use crate::{CaseMode, SearchConfig};

/// Deterministic xorshift generator: the next value below `bound`.
pub(super) fn random_below(state: &mut u64, bound: u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state % bound
}

fn random_bytes(length: u64, alphabet: &[u8], next: &mut impl FnMut(u64) -> u64) -> Vec<u8> {
    (0..length)
        .map(|_| alphabet[next(alphabet.len() as u64) as usize])
        .collect()
}

/// Scalar bounded-distance oracle, independent of the packed prefilter and prefix verifier.
fn verified_windows<'a>(
    pattern: &'a [u8],
    window: &'a [u8],
) -> impl Iterator<Item = (usize, u32)> + 'a {
    (pattern.len() - 1..=pattern.len() + 1)
        .filter(move |&length| length <= window.len())
        .filter_map(move |length| {
            levenshtein_naive_k(pattern, &window[..length], 1).map(|distance| (length, distance))
        })
}

#[test]
fn shared_prefix_windows_match_per_window_levenshtein() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = |bound| random_below(&mut state, bound);
    for _ in 0..20_000 {
        let pattern = random_bytes(4 + next(12), b"ab", &mut next);
        let window = random_bytes(next(pattern.len() as u64 + 2), b"ab", &mut next);
        // The unedited window first, then the longer before the shorter.
        let mut expected = verified_windows(&pattern, &window).collect::<Vec<_>>();
        expected.sort_unstable_by_key(|&(length, distance)| (distance, usize::MAX - length));
        // Trailing bytes past the window's reach must not affect the result.
        let mut text = window.clone();
        text.extend(random_bytes(16, b"ab", &mut next));
        let packed_window = u128::from_le_bytes(*text.first_chunk().expect("16 bytes follow"));
        let packed = PackedPattern::new(&pattern).expect("the pattern is shorter than 16 bytes");

        let mut actual = Vec::new();
        for_each_window_within_one_edit::<false>(&pattern, &window, |length, distance| {
            actual.push((length, distance));
        });
        let mut actual_packed = Vec::new();
        packed.for_each_window_within_one_edit::<false>(
            packed_window,
            window.len(),
            |length, distance| {
                actual_packed.push((length, distance));
            },
        );

        assert_eq!(actual, expected, "{pattern:?} {window:?}");
        assert_eq!(actual_packed, expected, "{pattern:?} {window:?}");
    }
}

#[test]
fn patterns_are_packed_unless_a_longer_window_has_no_room() {
    let packed = [1, 4, 15, 16, 40].map(|len| PackedPattern::new(&vec![b'a'; len]).is_some());

    assert_eq!(packed, [true, true, true, false, false]);
    assert_eq!(PackedPattern::new(b""), None);
}

#[test]
fn marked_passes_find_exactly_the_best_window_per_end_within_one_edit() {
    let mut state = 0x6a09_e667_f3bc_c908_u64;
    let mut next = |bound| random_below(&mut state, bound);
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    for round in 0..600 {
        // One or several groups, some patterns too long to pack. The small alphabet makes
        // halves alike, self-overlapping and sharing first bytes.
        let lengths = (0..1 + next(1 + round % 9))
            .map(|_| 4 + next(if round % 5 == 0 { 40 } else { 6 }))
            .collect::<Vec<_>>();
        let patterns = random_patterns(&lengths, &mut next);
        let mut haystack = random_bytes(next(1_500), b"abc\r\n", &mut next);
        if haystack.len() > 100 {
            plant_near_matches(&mut haystack, &patterns, &mut next);
        }
        let sample = PairSample::new(&haystack, false);
        let plan = MarkPlan::new(&patterns, sample.as_ref());
        // Varies independently of the lengths so long patterns meet every region size.
        let region_len = [haystack.len().max(1), 1, 3, 64, 257][(round / 5) as usize % 5];

        for line_local in [false, true] {
            let ctx = PrefilterContext {
                haystack: &haystack,
                folds: false,
                config,
                line_local,
            };
            let mut expected = best_windows_per_end(&patterns, &haystack, line_local);
            expected.sort_by(approximate_candidate_cmp);
            expected.sort_by_key(|candidate| candidate.matched.pattern_index);

            // Regroup per-region, per-pattern windows by pattern.
            let regions = (0..haystack.len())
                .step_by(region_len)
                .map(|first| {
                    let owned = first..(first + region_len).min(haystack.len());
                    let region = StartRegion::new(owned, haystack.len());
                    marked_candidates(&ctx, &region, &patterns, &plan)
                })
                .collect::<Vec<_>>();
            let actual = (0..patterns.len())
                .flat_map(|pattern| {
                    regions
                        .iter()
                        .flat_map(move |region| region[pattern].clone())
                })
                .collect::<Vec<_>>();

            assert_eq!(actual, expected, "{line_local} {patterns:?} {haystack:?}");
        }
    }
}

/// The sort-based end deduplication that the streaming pass replaces.
fn dedup_ends_by_sorting(mut candidates: Vec<ApproximateCandidate>) -> Vec<ApproximateCandidate> {
    candidates.sort_unstable_by(|left, right| {
        left.matched
            .end
            .cmp(&right.matched.end)
            .then(left.edit_distance.cmp(&right.edit_distance))
            .then(right.matched.start.cmp(&left.matched.start))
    });
    candidates.dedup_by(|read, written| read.matched.end == written.matched.end);
    candidates.sort_unstable_by(approximate_candidate_cmp);
    candidates
}

/// Returns, per pattern and end, the best window within one edit, in (end, pattern) order.
fn best_windows_per_end(
    patterns: &[ApproximatePattern],
    haystack: &[u8],
    line_local: bool,
) -> Vec<ApproximateCandidate> {
    let mut expected = Vec::new();
    for (pattern_index, pattern) in patterns.iter().enumerate() {
        let mut windows = Vec::new();
        for start in 0..haystack.len() {
            // Line content excludes the CR of a CRLF terminator, as `str::lines` does.
            let line_end = haystack[start..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(haystack.len(), |offset| {
                    let newline = start + offset;
                    if offset > 0 && haystack[newline - 1] == b'\r' {
                        newline - 1
                    } else {
                        newline
                    }
                });
            let limit = if line_local { line_end } else { haystack.len() };
            for (length, distance) in verified_windows(&pattern.bytes, &haystack[start..limit]) {
                windows.push(ApproximateCandidate::new(
                    start,
                    start + length,
                    pattern_index,
                    distance,
                ));
            }
        }
        expected.extend(dedup_ends_by_sorting(windows));
    }
    expected
        .sort_unstable_by_key(|candidate| (candidate.matched.end, candidate.matched.pattern_index));
    expected
}

/// Overwrites random spots of `haystack` with patterns carrying at most one random edit, so
/// long patterns match too.
fn plant_near_matches(
    haystack: &mut Vec<u8>,
    patterns: &[ApproximatePattern],
    next: &mut impl FnMut(u64) -> u64,
) {
    for _ in 0..haystack.len() / 40 {
        let mut planted = patterns[next(patterns.len() as u64) as usize].bytes.clone();
        let edit_at = next(planted.len() as u64) as usize;
        match next(4) {
            0 => planted[edit_at] = b'a',
            1 => {
                planted.remove(edit_at);
            }
            2 => planted.insert(edit_at, b'b'),
            _ => {}
        }
        let at = next((haystack.len() - planted.len()) as u64) as usize;
        haystack.splice(at..at + planted.len(), planted);
    }
}

/// Returns one random pattern per entry of `lengths`.
fn random_patterns(lengths: &[u64], next: &mut impl FnMut(u64) -> u64) -> Vec<ApproximatePattern> {
    lengths
        .iter()
        .map(|&length| {
            let bytes = random_bytes(length, b"abc\r\n", &mut *next);
            ApproximatePattern::new(bytes, 1)
        })
        .collect()
}

#[test]
fn bitap_finds_exactly_the_best_window_per_end_within_one_edit() {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = |bound| random_below(&mut state, bound);
    for _ in 0..2_000 {
        let lengths = (0..1 + next(3)).map(|_| 4 + next(5)).collect::<Vec<_>>();
        let patterns = random_patterns(&lengths, &mut next);
        let haystack = random_bytes(next(400), b"abc\r\n", &mut next);
        let bitap = BitapPrefilter::build(&patterns, false).expect("short patterns fit in 64 bits");
        let config = SearchConfig::builder().max_edit_distance(Some(1)).build();

        for line_local in [false, true] {
            let ctx = PrefilterContext {
                haystack: &haystack,
                folds: false,
                config,
                line_local,
            };
            let actual = bitap.region_windows(&ctx, 0..haystack.len());

            let mut expected = best_windows_per_end(&patterns, &haystack, line_local);
            assert_eq!(actual, expected, "{line_local} {haystack:?}");

            let mut sorted = actual;
            sort_nearly_sorted(&mut sorted);
            expected.sort_unstable_by(approximate_candidate_cmp);
            assert_eq!(sorted, expected, "{line_local} {haystack:?}");
        }
    }
}

#[test]
fn every_lane_layout_finds_the_owned_windows() {
    // Pattern bits of 4, 7, 15, 27, 33 and 64 pack 16, 8, 4, 2, 1 and 1 copies per word.
    let length_sets: [&[u64]; 6] = [
        &[4],
        &[7],
        &[5, 6, 4],
        &[9, 9, 9],
        &[16, 17],
        &[13, 13, 13, 13, 12],
    ];
    let mut state = 0x3c6e_f372_fe94_f82b_u64;
    let mut next = |bound| random_below(&mut state, bound);
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    for lengths in length_sets {
        for _ in 0..8 {
            let patterns = random_patterns(lengths, &mut next);
            let bitap = BitapPrefilter::build(&patterns, false).expect("patterns fit in 64 bits");
            let mut haystack = random_bytes(600 + next(900), b"abc\r\n", &mut next);
            plant_near_matches(&mut haystack, &patterns, &mut next);
            let owned_start = next(haystack.len() as u64 / 4) as usize;
            let owned_end = haystack.len() - next(haystack.len() as u64 / 4) as usize;

            for line_local in [false, true] {
                let ctx = PrefilterContext {
                    haystack: &haystack,
                    folds: false,
                    config,
                    line_local,
                };

                let actual = bitap.region_windows(&ctx, owned_start..owned_end);

                let mut expected = best_windows_per_end(&patterns, &haystack, line_local);
                expected.retain(|candidate| {
                    (owned_start..owned_end).contains(&candidate.matched.start)
                });
                // Patterns holding a newline cannot match within one line.
                assert!(
                    line_local || !expected.is_empty(),
                    "{lengths:?}: planted windows must match"
                );
                assert_eq!(actual, expected, "{lengths:?} {line_local}");
            }
        }
    }
}

#[test]
fn region_split_search_matches_one_region() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let haystack = (0..3 * MIN_DENSE_REGION_LEN + 77)
        .map(|_| b"acgtacgtacgtacgt \r\n"[random_below(&mut state, 19) as usize])
        .collect::<Vec<u8>>();
    let pattern_sets: [&[&[u8]]; 2] = [&[b"acgtac"], &[b"acgtac", b"ttgca", b"gggaccat"]];
    const REGION_LEN: usize = 1_000;

    for patterns in pattern_sets {
        let patterns = patterns
            .iter()
            .map(|pattern| pattern.to_vec())
            .collect::<Vec<_>>();
        for whole_word in [false, true] {
            let config = SearchConfig::builder()
                .max_edit_distance(Some(1))
                .whole_word(whole_word)
                .build();
            let engine = ApproximateEngine::new(&patterns, config).expect("engine builds");
            for (line_local, use_bitap) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                let ctx = PrefilterContext {
                    haystack: &haystack,
                    folds: false,
                    config,
                    line_local,
                };
                let bitap = engine.bitap.as_deref().filter(|_| use_bitap);

                let marks = &engine.marks;
                let whole = engine.search_region(&ctx, bitap, marks, 0..haystack.len());
                let split = (0..haystack.len())
                    .step_by(REGION_LEN)
                    .flat_map(|first| {
                        let last = (first + REGION_LEN).min(haystack.len());
                        engine.search_region(&ctx, bitap, marks, first..last)
                    })
                    .collect::<Vec<_>>();

                assert!(!whole.is_empty());
                assert_eq!(
                    split, whole,
                    "{patterns:?} {whole_word} {line_local} {use_bitap}"
                );
                // Both candidate sources must find the same windows.
                assert_eq!(
                    whole,
                    engine.search_region(&ctx, None, marks, 0..haystack.len()),
                    "{patterns:?} {whole_word} {line_local}"
                );
            }
        }
    }
}

#[test]
fn merge_keeps_the_best_candidate_per_start() {
    let mut state = 0x2f69_3a1c_5b7e_d40b_u64;
    let mut next = |bound| random_below(&mut state, bound) as usize;
    let per_pattern = (0..4)
        .map(|pattern_index| {
            let mut candidates = Vec::new();
            for start in 0..3_000 {
                if next(3) == 0 {
                    candidates.push(ApproximateCandidate::new(
                        start,
                        start + 4 + next(3),
                        pattern_index,
                        next(2) as u32,
                    ));
                }
            }
            candidates
        })
        .collect::<Vec<_>>();
    let mut expected = per_pattern.concat();
    expected.sort_unstable_by(approximate_candidate_cmp);
    let deduplicated = ApproximateCandidate::dedup_starts(expected.clone());
    expected.dedup_by_key(|candidate| candidate.matched.start);
    assert_eq!(deduplicated, expected);

    let merged = merge_sorted_runs(per_pattern.iter().map(Vec::as_slice).collect());

    assert_eq!(merged, expected);
}

#[test]
fn streaming_end_dedup_matches_sorting_reference() {
    let mut state = 0x853c_49e6_748f_ea9b_u64;
    let mut next = |bound| random_below(&mut state, bound);
    for _ in 0..2_000 {
        let pattern = random_bytes(4 + next(4), b"ab", &mut next);
        let haystack = random_bytes(next(80), b"ab", &mut next);
        // Every verified window in ascending start order.
        let candidates = (0..haystack.len())
            .flat_map(|start| {
                verified_windows(&pattern, &haystack[start..]).map(
                    move |(length, edit_distance)| {
                        ApproximateCandidate::new(start, start + length, 0, edit_distance)
                    },
                )
            })
            .collect::<Vec<_>>();

        let mut streamed = candidates.clone();
        // The order the windows of a start are found in.
        streamed.sort_by(approximate_candidate_cmp);
        dedup_ends_in_start_order(&mut streamed);

        assert_eq!(streamed, dedup_ends_by_sorting(candidates));
    }
}

#[test]
fn stitched_region_selections_equal_one_serial_selection() {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    // Dense, overlapping candidates in start order, several per start.
    let mut candidates = (0..600_usize)
        .flat_map(|start| (0..3).map(move |extra| (start, extra)))
        .filter(|_| random_below(&mut state, 3) == 0)
        .map(|(start, extra)| ApproximateCandidate::new(start, start + 3 + extra, 0, 1))
        .collect::<Vec<_>>();
    candidates.sort_by(approximate_candidate_cmp);

    let config = SearchConfig::default();
    let expected = select_approximate_matches(candidates.iter().copied(), config, usize::MAX);
    for region_len in [1, 2, 5, 17, 64, 600] {
        let mut matches = Vec::new();
        let mut last_end = 0;
        for region in candidates.chunk_by(|left, right| {
            left.matched.start / region_len == right.matched.start / region_len
        }) {
            let speculative =
                select_approximate_matches(region.iter().copied(), config, usize::MAX);
            stitch_region_selection(&mut matches, &mut last_end, region, &speculative);
        }

        assert_eq!(matches, expected, "regions of {region_len}");
    }
}

#[test]
fn overlapping_selection_keeps_every_candidate_up_to_the_limit() {
    let candidates = (0..40_usize)
        .map(|start| ApproximateCandidate::new(start / 2, start / 2 + 3 + start % 2, 0, 1))
        .collect::<Vec<_>>();
    let config = SearchConfig::builder().overlapping(true).build();

    for limit in [1, 7, 40, usize::MAX] {
        let selected = select_approximate_matches(candidates.iter().copied(), config, limit);

        let expected = candidates
            .iter()
            .take(limit)
            .map(|candidate| candidate.matched)
            .collect::<Vec<_>>();
        assert_eq!(selected, expected, "limit {limit}");
    }
}

#[test]
fn nearly_sorted_sort_matches_a_full_sort_at_any_disorder() {
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    for max_shift in [0, 2, 8, 64, 4_000] {
        // Starts trail end-ordered ends by up to `max_shift` bytes; large shifts exhaust the
        // insertion sort's move budget and exercise the fallback.
        let mut candidates = (0..4_000_usize)
            .map(|end| {
                let start = end.saturating_sub(random_below(&mut state, max_shift + 1) as usize);
                ApproximateCandidate::new(start, end + 4, 0, 1)
            })
            .collect::<Vec<_>>();
        let mut expected = candidates.clone();
        expected.sort_unstable_by(approximate_candidate_cmp);

        sort_nearly_sorted(&mut candidates);

        assert_eq!(candidates, expected, "max shift {max_shift}");
    }
}

#[test]
fn lane_strides_near_a_cache_alias_multiple_still_cover_every_position() {
    let mut state = 0xa54f_f53a_5f1d_36f1_u64;
    let mut next = |bound| random_below(&mut state, bound);
    // One four-byte pattern packs 16 copies; 16 lanes of 4100 bytes put the stride within a
    // cache line of 4096, so it is shortened and the leftover is scanned recursively.
    let patterns = random_patterns(&[4], &mut next);
    let bitap = BitapPrefilter::build(&patterns, false).expect("one short pattern fits in 64 bits");
    let mut haystack = random_bytes(16 * 4_100 - 4, b"abc\r\n", &mut next);
    plant_near_matches(&mut haystack, &patterns, &mut next);
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let ctx = PrefilterContext {
        haystack: &haystack,
        folds: false,
        config,
        line_local: false,
    };

    let actual = bitap.region_windows(&ctx, 0..haystack.len());

    assert_eq!(actual, best_windows_per_end(&patterns, &haystack, false));
}

#[test]
fn region_layouts_split_every_start_into_consecutive_regions() {
    for (haystack_len, region_len) in [(1, 64), (64, 64), (65, 64), (1_000, 128)] {
        let regions = RegionLayout {
            haystack_len,
            region_len,
        };

        let starts = (0..regions.count())
            .flat_map(|region_index| regions.starts(region_index))
            .collect::<Vec<_>>();

        assert_eq!(starts, (0..haystack_len).collect::<Vec<_>>());
        assert_eq!(regions.count(), haystack_len.div_ceil(region_len));
    }
}

/// Random DNA, dense in any DNA pattern's marks, and sparse text with a few near matches of
/// `acgtac` and `sttus=500`.
fn dense_and_sparse_text() -> (Vec<u8>, Vec<u8>) {
    let mut state = 0x7f4a_7c15_9e37_79b9_u64;
    let dense = (0..40_000)
        .map(|_| b"acgt\n"[random_below(&mut state, 5) as usize])
        .collect();
    let filler = "- - - - - - - - - - - - - - - - - - - - - - - - - -\n".repeat(4);
    let sparse = format!("{filler}sample acgtaac read\nsttus=50 seen\n")
        .repeat(200)
        .into_bytes();
    (dense, sparse)
}

#[test]
fn text_dense_in_marks_is_left_to_the_bit_parallel_pass_which_finds_the_same_matches() {
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let engine = ApproximateEngine::new(&[b"acgtac".to_vec()], config).expect("engine builds");
    let bitap = engine.bitap.as_deref().expect("the pattern fits a word");
    let (dense, sparse) = dense_and_sparse_text();

    for (text, expect_dense) in [(&dense, true), (&sparse, false)] {
        let sample = PairSample::new(text, false).expect("the text is long enough to sample");
        let marks = MarkPlan::new(&engine.patterns, Some(&sample));
        let ctx = PrefilterContext {
            haystack: text,
            folds: false,
            config,
            line_local: false,
        };
        let selected = |bitap: Option<&BitapPrefilter>| {
            let candidates = engine.search_region(&ctx, bitap, &marks, 0..text.len());
            select_approximate_matches(candidates, config, usize::MAX)
        };

        let searched = engine.search(text, config, usize::MAX);

        assert_eq!(
            marks.stops_within(bitap.stop_spacing(), &sample),
            expect_dense
        );
        assert!(!searched.is_empty());
        assert_eq!(searched, selected(Some(bitap)), "{expect_dense}");
        assert_eq!(searched, selected(None), "{expect_dense}");
    }
}

#[test]
fn line_plans_are_made_only_for_lines_one_sparse_pass_counts() {
    let plain = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let whole_word = SearchConfig::builder()
        .max_edit_distance(Some(1))
        .whole_word(true)
        .build();
    let (dense, sparse) = dense_and_sparse_text();
    let engine = |patterns: &[&str]| {
        let patterns = patterns
            .iter()
            .map(|pattern| pattern.as_bytes().to_vec())
            .collect::<Vec<_>>();
        ApproximateEngine::new(&patterns, plain).expect("engine builds")
    };
    // Too long for the bit-parallel pass, so dense marks can't disqualify the plan.
    let long_pattern = "acgt".repeat(17);

    let planned = engine(&["acgtac", "sttus=500"]);
    let plan = planned.line_count_plan(&sparse, plain);
    let dense_plan = planned.line_count_plan(&dense, plain);
    let whole_word_plan = planned.line_count_plan(&sparse, whole_word);
    // `abc` has no halves, and the first half of `EROR` is that of `ERORS`.
    let unhalved_plan = engine(&["acgtac", "abc"]).line_count_plan(&sparse, plain);
    let alike_plan = engine(&["EROR", "ERORS"]).line_count_plan(&sparse, plain);
    let long_plan = engine(&[&long_pattern]).line_count_plan(&dense, plain);

    let plan = plan.expect("sparse marks of halves that differ are counted in one pass");
    assert_eq!(
        planned.count_planned_lines(&sparse, plain, &plan),
        planned.count_searched_lines(&sparse, plain)
    );
    assert_eq!(planned.count_planned_lines(&sparse, plain, &plan), 400);
    assert_eq!(dense_plan, None);
    assert_eq!(whole_word_plan, None);
    assert_eq!(unhalved_plan, None);
    assert_eq!(alike_plan, None);
    assert!(long_plan.is_some());
}

#[test]
fn stops_have_to_be_sparser_the_more_copies_the_bit_parallel_pass_packs() {
    // Patterns of 40, 20, 16, 8 and 4 bits: 1, 2, 4, 8 and 16 copies in a word.
    let spacing = [40, 20, 16, 8, 4].map(|length| {
        let pattern = ApproximatePattern::new(vec![b'a'; length], 1);
        let bitap = BitapPrefilter::build(&[pattern], false).expect("the pattern fits a word");
        bitap.stop_spacing()
    });

    assert_eq!(spacing, [12, 24, 36, 36, 36]);
}

#[test]
fn patterns_with_and_without_halves_are_searched_by_region_and_by_pattern() {
    let config = SearchConfig::builder()
        .max_edit_distance(Some(1))
        .overlapping(true)
        .build();
    // `abc` and `xyz` are too short for halves.
    let mixed = [&b"abc"[..], b"acgtac", b"xyz", b"ttgca"].map(<[u8]>::to_vec);
    let unhalved = [&b"abc"[..], b"xyz"].map(<[u8]>::to_vec);
    let mut state = 0x3c6e_f372_fe94_f82b_u64;
    let haystack = (0..3 * MIN_SPARSE_REGION_LEN + 77)
        .map(|_| b"acgtabcxyz \n"[random_below(&mut state, 12) as usize])
        .collect::<Vec<u8>>();
    let pool = |threads| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("a thread pool should build")
    };

    for patterns in [&mixed[..], &unhalved] {
        let engine = ApproximateEngine::new(patterns, config).expect("engine builds");
        // The windows every pattern has alone: several per start.
        let alone = patterns
            .iter()
            .enumerate()
            .flat_map(|(pattern_index, pattern)| {
                let pattern = std::slice::from_ref(pattern);
                let engine = ApproximateEngine::new(pattern, config).expect("engine builds");
                let matches = engine.search(&haystack, config, usize::MAX);
                matches
                    .into_iter()
                    .map(move |matched| (matched.start, matched.end, pattern_index))
            })
            .collect::<HashSet<_>>();
        let starts = alone
            .iter()
            .map(|&(start, _, _)| start)
            .collect::<HashSet<_>>();

        let (in_regions, region_count) = pool(4).install(|| {
            let regions = engine.region_layout(haystack.len(), false);
            (
                engine.search(&haystack, config, usize::MAX),
                regions.count(),
            )
        });
        let in_order = pool(1).install(|| engine.search(&haystack, config, usize::MAX));

        assert!(region_count > 1);

        // One match per start that has a window, each a window of its pattern alone.
        assert_eq!(in_regions.len(), starts.len());
        assert!(in_regions.len() > 1_000);
        assert!(
            in_regions
                .windows(2)
                .all(|pair| pair[0].start < pair[1].start)
        );
        assert!(in_regions.iter().all(|matched| {
            alone.contains(&(matched.start, matched.end, matched.pattern_index))
        }));
        assert_eq!(in_regions, in_order);
    }
}

#[test]
fn case_is_ignored_as_if_the_haystack_were_lowered_first() {
    let mut state = 0x243f_6a88_85a3_08d3_u64;
    let mut next = |bound| random_below(&mut state, bound);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("a small pool starts");
    // Bytes next to the letters differ from them only in the bit that tells case apart.
    let sparse = b"abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ@[`{\n";
    let random_texts = [
        // Regions of the mark pass, with packed and unpacked patterns.
        (
            &sparse[..],
            600_000,
            vec![
                "Needle",
                "QUARTZ",
                "An Unusually Long Pattern, Too Long To Pack",
            ],
        ),
        // Text dense in marks, left to the bit-parallel pass.
        (&b"abAB\n"[..], 200_000, vec!["aBbA", "BABAAB"]),
        // A pattern without halves, searched in a lowered copy.
        (&sparse[..], 40_000, vec!["Needle", "aBc"]),
        // Marks with letters whose first bytes differ only in the bit that tells case
        // apart, scanned in lowered blocks.
        (&sparse[..], 40_000, vec!["@a@a@a", "`b`b`b"]),
        // Marks without a letter, scanned as the text is written.
        (&sparse[..], 40_000, vec!["@@@@@@", "``````"]),
    ];
    let mut texts = random_texts
        .into_iter()
        .map(|(alphabet, length, patterns)| {
            let mut haystack = random_bytes(length, alphabet, &mut next);
            let planted = patterns
                .iter()
                .map(|pattern| ApproximatePattern::new(pattern.as_bytes().to_vec(), 1))
                .collect::<Vec<_>>();
            plant_near_matches(&mut haystack, &planted, &mut next);
            for byte in &mut haystack {
                if next(3) == 0 {
                    *byte = byte.to_ascii_uppercase();
                }
            }
            (haystack, patterns)
        })
        .collect::<Vec<_>>();
    // Too short to pack a window anywhere.
    texts.push((b"a NEedlE z".to_vec(), vec!["Needle"]));

    for (haystack, patterns) in texts {
        let length = haystack.len();
        let lowered = haystack.to_ascii_lowercase();
        let raw_patterns = |lower: bool| {
            patterns
                .iter()
                .map(|pattern| {
                    let pattern = pattern.as_bytes();
                    if lower {
                        pattern.to_ascii_lowercase()
                    } else {
                        pattern.to_vec()
                    }
                })
                .collect::<Vec<_>>()
        };
        for (overlapping, whole_word) in [(false, false), (true, false), (false, true)] {
            let config = |case_mode| {
                SearchConfig::builder()
                    .max_edit_distance(Some(1))
                    .case_mode(case_mode)
                    .overlapping(overlapping)
                    .whole_word(whole_word)
                    .build()
            };
            let ignoring = config(CaseMode::AsciiInsensitive);
            let exact = config(CaseMode::Sensitive);
            let ignoring_engine =
                ApproximateEngine::new(&raw_patterns(false), ignoring).expect("valid distance");
            let exact_engine =
                ApproximateEngine::new(&raw_patterns(true), exact).expect("valid distance");
            let lines = |engine: &ApproximateEngine, text: &[u8], config| {
                if !engine.supports_line_local_search() {
                    return None;
                }
                let planned = engine
                    .line_count_plan(text, config)
                    .map(|plan| engine.count_planned_lines(text, config, &plan));
                Some((
                    engine.search_line_local(text, config),
                    engine.count_searched_lines(text, config),
                    planned,
                ))
            };

            let (found, expected, found_lines, expected_lines) = pool.install(|| {
                (
                    ignoring_engine.search(&haystack, ignoring, usize::MAX),
                    exact_engine.search(&lowered, exact, usize::MAX),
                    lines(&ignoring_engine, &haystack, ignoring),
                    lines(&exact_engine, &lowered, exact),
                )
            });

            assert!(
                !expected.is_empty(),
                "{patterns:?} should match in {length} bytes"
            );
            assert_eq!(found, expected, "{patterns:?} in {length} bytes");
            assert_eq!(
                found_lines, expected_lines,
                "{patterns:?} in {length} bytes"
            );
        }
    }
}
