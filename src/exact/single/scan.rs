use memchr::{memchr_iter, memchr2_iter};

use super::{SinglePatternEngine, single_match, visit_single_byte_candidates};
use crate::{
    Match, SearchConfig, fold_ascii, is_word_boundary, profiles, unfold_ascii,
    window_matches_ascii_insensitive, word_run_len,
};

/// Bytes folded per window by ASCII-insensitive memmem search: small enough to stay in cache
/// between folding and searching.
const FOLDED_WINDOW_LEN: usize = 64 * 1024;

impl SinglePatternEngine {
    /// Visits accepted matches with the chosen strategy; config flags specialize before the
    /// per-hit loop. Returning `false` from the callback stops the scan.
    #[inline(always)]
    pub(super) fn visit_matches<const COLLECT: bool>(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        strategy: profiles::SingleStrategy,
        mut visit: impl FnMut(Match) -> bool,
    ) {
        if haystack.len() < self.pattern.len() {
            return;
        }
        if self.pattern.len() == 1 {
            visit_single_byte_candidates(haystack, self.pattern[0], config, |start| {
                visit(single_match(start, start + 1))
            });
            return;
        }
        match (config.whole_word(), config.overlapping()) {
            (false, false) => {
                self.visit_configured::<COLLECT, false, false>(haystack, strategy, visit)
            }
            (false, true) => {
                self.visit_configured::<COLLECT, false, true>(haystack, strategy, visit)
            }
            (true, false) => {
                self.visit_configured::<COLLECT, true, false>(haystack, strategy, visit)
            }
            (true, true) => self.visit_configured::<COLLECT, true, true>(haystack, strategy, visit),
        }
    }

    #[inline(always)]
    fn visit_configured<const COLLECT: bool, const WHOLE_WORD: bool, const OVERLAPPING: bool>(
        &self,
        haystack: &[u8],
        strategy: profiles::SingleStrategy,
        visit: impl FnMut(Match) -> bool,
    ) {
        match strategy {
            profiles::SingleStrategy::SensitiveMemmem => {
                self.visit_memmem::<WHOLE_WORD, OVERLAPPING>(haystack, haystack, 0, visit);
            }
            profiles::SingleStrategy::SensitiveAnchored if COLLECT && OVERLAPPING => {
                self.visit_sensitive_overlapping::<WHOLE_WORD>(haystack, visit);
            }
            profiles::SingleStrategy::SensitiveAnchored => {
                self.visit_anchor::<false, WHOLE_WORD, OVERLAPPING>(haystack, visit);
            }
            profiles::SingleStrategy::AsciiAnchored => {
                self.visit_anchor::<true, WHOLE_WORD, OVERLAPPING>(haystack, visit);
            }
            profiles::SingleStrategy::AsciiFoldedMemmem if COLLECT && haystack.len() < 256 => {
                self.visit_bmh_ascii_insensitive::<WHOLE_WORD, OVERLAPPING>(haystack, visit);
            }
            profiles::SingleStrategy::AsciiFoldedMemmem => {
                self.visit_folded_windows::<WHOLE_WORD, OVERLAPPING>(haystack, visit);
            }
        }
    }

    /// Folds and searches `haystack` one cache-sized window at a time, bounding memory. Each
    /// window overlaps the previous by a pattern length less one, so seam matches are found
    /// exactly once.
    #[inline(always)]
    fn visit_folded_windows<const WHOLE_WORD: bool, const OVERLAPPING: bool>(
        &self,
        haystack: &[u8],
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let carry = self.pattern.len() - 1;
        // At least two pattern lengths, so every window advances past the carried tail.
        let window_len = FOLDED_WINDOW_LEN.max(2 * self.pattern.len());
        let mut folded = Vec::with_capacity(window_len.min(haystack.len()));
        let mut start = 0;
        loop {
            let end = (start + window_len).min(haystack.len());
            folded.clear();
            folded.extend(haystack[start..end].iter().copied().map(fold_ascii));
            // `&mut visit` lends the callback to each window instead of moving it into the first.
            let Some(resume) =
                self.visit_memmem::<WHOLE_WORD, OVERLAPPING>(haystack, &folded, start, &mut visit)
            else {
                return;
            };
            if end == haystack.len() {
                return;
            }
            start = resume.max(end - carry);
        }
    }

    /// Visits occurrences in `normalized`, the (possibly folded) bytes of `haystack` from
    /// `offset`. Returns the `haystack` offset to resume at, or `None` if `visit` stopped.
    #[inline(always)]
    fn visit_memmem<const WHOLE_WORD: bool, const OVERLAPPING: bool>(
        &self,
        haystack: &[u8],
        normalized: &[u8],
        offset: usize,
        mut visit: impl FnMut(Match) -> bool,
    ) -> Option<usize> {
        let pattern_len = self.pattern.len();
        let mut resume = 0;
        if OVERLAPPING || WHOLE_WORD {
            while let Some(relative) = self.finder.find(&normalized[resume..]) {
                let relative_start = resume + relative;
                let start = offset + relative_start;
                let end = start + pattern_len;
                let rejected = WHOLE_WORD && !is_word_boundary(haystack, start, end);
                if !rejected && !visit(single_match(start, end)) {
                    return None;
                }
                resume = if rejected {
                    // The next whole word starts past this run of word bytes, so skip it
                    // instead of retrying at each byte. Stop at the window's end so a long
                    // run is not reread by every window.
                    let window_end = offset + normalized.len();
                    let passed = relative_start + word_run_len(&haystack[start..window_end]) + 1;
                    passed.min(normalized.len())
                } else if OVERLAPPING {
                    // An overlapping match gives up only its first byte.
                    relative_start + 1
                } else {
                    relative_start + pattern_len
                };
            }
        } else {
            for relative_start in self.finder.find_iter(normalized) {
                let start = offset + relative_start;
                if !visit(single_match(start, start + pattern_len)) {
                    return None;
                }
                resume = relative_start + pattern_len;
            }
        }
        Some(offset + resume)
    }

    /// Case, boundary filtering, and overlap decisions are constant within each scan loop.
    #[inline(always)]
    fn visit_anchor<const INSENSITIVE: bool, const WHOLE_WORD: bool, const OVERLAPPING: bool>(
        &self,
        haystack: &[u8],
        visit: impl FnMut(Match) -> bool,
    ) {
        let (anchor_index, anchor_byte) = self.anchor;
        let anchor_upper = unfold_ascii(anchor_byte);
        // Positions in the haystack offset by the anchor index are candidate match starts.
        let anchors = &haystack[anchor_index..];
        if INSENSITIVE && anchor_byte != anchor_upper {
            self.visit_anchor_starts::<INSENSITIVE, WHOLE_WORD, OVERLAPPING>(
                haystack,
                memchr2_iter(anchor_byte, anchor_upper, anchors),
                visit,
            );
        } else {
            self.visit_anchor_starts::<INSENSITIVE, WHOLE_WORD, OVERLAPPING>(
                haystack,
                memchr_iter(anchor_byte, anchors),
                visit,
            );
        }
    }

    /// Verifies candidate starts from one resumable SIMD iterator, skipping candidates that
    /// overlap an accepted non-overlapping match.
    #[inline(always)]
    fn visit_anchor_starts<
        const INSENSITIVE: bool,
        const WHOLE_WORD: bool,
        const OVERLAPPING: bool,
    >(
        &self,
        haystack: &[u8],
        starts: impl Iterator<Item = usize>,
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let pattern_len = self.pattern.len();
        let mut next_start = 0_usize;
        for start in starts {
            if !OVERLAPPING && start < next_start {
                continue;
            }
            let end = start + pattern_len;
            if end > haystack.len() {
                break;
            }
            if self.accepts_anchor::<INSENSITIVE, WHOLE_WORD>(haystack, start, end) {
                if !visit(single_match(start, end)) {
                    break;
                }
                next_start = end;
            }
        }
    }

    #[inline(always)]
    fn accepts_anchor<const INSENSITIVE: bool, const WHOLE_WORD: bool>(
        &self,
        haystack: &[u8],
        start: usize,
        end: usize,
    ) -> bool {
        // The cheap two-byte boundary test goes first.
        (!WHOLE_WORD || is_word_boundary(haystack, start, end))
            && if INSENSITIVE {
                window_matches_ascii_insensitive(&haystack[start..end], &self.pattern)
            } else {
                haystack[start..end] == *self.pattern
            }
    }

    /// Keeps the single SIMD iterator pass used by overlapping sensitive result collection.
    #[inline(always)]
    fn visit_sensitive_overlapping<const WHOLE_WORD: bool>(
        &self,
        haystack: &[u8],
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let (anchor_index, anchor_byte) = self.anchor;
        for anchor_position in memchr_iter(anchor_byte, haystack) {
            if anchor_position < anchor_index {
                continue;
            }
            let start = anchor_position - anchor_index;
            let end = start + self.pattern.len();
            if end > haystack.len() {
                break;
            }
            if self.accepts_anchor::<false, WHOLE_WORD>(haystack, start, end)
                && !visit(single_match(start, end))
            {
                break;
            }
        }
    }

    #[inline(always)]
    fn visit_bmh_ascii_insensitive<const WHOLE_WORD: bool, const OVERLAPPING: bool>(
        &self,
        haystack: &[u8],
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let pattern_len = self.pattern.len();
        let last = pattern_len - 1;
        let mut index = 0;
        while index + pattern_len <= haystack.len() {
            let tail = fold_ascii(haystack[index + last]);
            if tail == self.pattern[last]
                && window_matches_ascii_insensitive(
                    &haystack[index..index + pattern_len],
                    &self.pattern,
                )
            {
                let end = index + pattern_len;
                if !WHOLE_WORD || is_word_boundary(haystack, index, end) {
                    if !visit(single_match(index, end)) {
                        break;
                    }
                    index += if OVERLAPPING { 1 } else { pattern_len };
                } else {
                    index += 1;
                }
            } else {
                index += self.shift[usize::from(tail)].max(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaseMode;

    #[test]
    fn visitors_agree_for_boundaries_overlaps_and_case_modes() {
        for case_mode in [CaseMode::Sensitive, CaseMode::AsciiInsensitive] {
            for overlapping in [false, true] {
                for whole_word in [false, true] {
                    let config = SearchConfig::builder()
                        .case_mode(case_mode)
                        .overlapping(overlapping)
                        .whole_word(whole_word)
                        .build();
                    for (pattern, haystack) in [
                        (b"aba".as_slice(), b"ababa ABA aba\r\nxaba aba".as_slice()),
                        (b"a\na", b"a\na\na\na"),
                        (b"\naa", b"\naa\naa\n"),
                        (b"a\r\n", b"a\r\na\r\n"),
                        (b"aa", b"aaaa aa\r\naaaa\n"),
                    ] {
                        let engine = SinglePatternEngine::new(pattern.to_vec());
                        let pattern_len = pattern.len();
                        let normalized = crate::normalize_haystack(haystack, case_mode);
                        let mut anchored = Vec::new();
                        let collect = |matched| {
                            anchored.push(matched);
                            true
                        };
                        let strategy = match case_mode {
                            CaseMode::Sensitive => profiles::SingleStrategy::SensitiveAnchored,
                            CaseMode::AsciiInsensitive => profiles::SingleStrategy::AsciiAnchored,
                        };
                        engine.visit_matches::<false>(haystack, config, strategy, collect);
                        let mut expected = Vec::new();
                        let mut offset = 0;
                        while offset + pattern_len <= haystack.len() {
                            if normalized[offset..offset + pattern_len] == *pattern
                                && (!whole_word || is_word_boundary(haystack, offset, offset + 3))
                            {
                                expected.push(single_match(offset, offset + pattern_len));
                                offset += if overlapping { 1 } else { pattern_len };
                            } else {
                                offset += 1;
                            }
                        }
                        assert_eq!(anchored, expected);
                        if overlapping && case_mode == CaseMode::Sensitive {
                            let mut iterated = Vec::new();
                            engine.visit_matches::<true>(haystack, config, strategy, |matched| {
                                iterated.push(matched);
                                true
                            });
                            assert_eq!(iterated, expected);
                        }
                        let mut bmh = Vec::new();
                        engine.visit_matches::<true>(
                            haystack,
                            config,
                            profiles::SingleStrategy::AsciiFoldedMemmem,
                            |matched| {
                                bmh.push(matched);
                                true
                            },
                        );
                        if case_mode == CaseMode::AsciiInsensitive {
                            assert_eq!(bmh, expected);
                        }
                        assert_eq!(
                            engine.find_first(haystack, config),
                            expected.first().copied()
                        );
                        for limit in [0, 1, 2, usize::MAX] {
                            assert_eq!(
                                engine.search(haystack, config, limit),
                                expected[..expected.len().min(limit)]
                            );
                            assert_eq!(
                                engine.count(haystack, config, limit),
                                expected.len().min(limit)
                            );
                        }
                        assert_eq!(
                            engine.count_matching_lines(haystack, config),
                            expected
                                .iter()
                                .map(|matched| memchr_iter(b'\n', &haystack[..matched.start])
                                    .count())
                                .collect::<std::collections::BTreeSet<_>>()
                                .len(),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn whole_word_memmem_finds_occurrences_overlapping_rejected_ones() {
        // The occurrence at 1 follows a word byte and is rejected; the one at 4 overlaps it.
        // The filler keeps the folded strategy on its windowed memmem path.
        let mut haystack = b"xab ab ab ".to_vec();
        haystack.extend(std::iter::repeat_n(b'.', 300));
        let engine = SinglePatternEngine::new(b"ab ab".to_vec());

        for (case_mode, strategy) in [
            (
                CaseMode::Sensitive,
                profiles::SingleStrategy::SensitiveMemmem,
            ),
            (
                CaseMode::AsciiInsensitive,
                profiles::SingleStrategy::AsciiFoldedMemmem,
            ),
        ] {
            let config = SearchConfig::builder()
                .case_mode(case_mode)
                .whole_word(true)
                .build();
            let mut found = Vec::new();

            engine.visit_matches::<true>(&haystack, config, strategy, |matched| {
                found.push(matched);
                true
            });

            assert_eq!(found, [single_match(4, 9)], "{case_mode:?}");
        }
    }

    #[test]
    fn whole_word_memmem_passes_runs_of_word_bytes_after_a_rejection() {
        // A run spanning several folded windows holds only non-word occurrences; two whole
        // words follow, then an occurrence inside a longer word.
        let run = 3 * FOLDED_WINDOW_LEN + 5;
        let mut haystack = vec![b'a'; run];
        haystack.extend_from_slice(b" aaaa-aaaa aaaab");
        haystack.extend(std::iter::repeat_n(b'.', 300));
        let engine = SinglePatternEngine::new(b"aaaa".to_vec());

        for (case_mode, strategy) in [
            (
                CaseMode::Sensitive,
                profiles::SingleStrategy::SensitiveMemmem,
            ),
            (
                CaseMode::AsciiInsensitive,
                profiles::SingleStrategy::AsciiFoldedMemmem,
            ),
        ] {
            for overlapping in [false, true] {
                let config = SearchConfig::builder()
                    .case_mode(case_mode)
                    .whole_word(true)
                    .overlapping(overlapping)
                    .build();
                let mut found = Vec::new();

                engine.visit_matches::<true>(&haystack, config, strategy, |matched| {
                    found.push(matched);
                    true
                });

                assert_eq!(
                    found,
                    [
                        single_match(run + 1, run + 5),
                        single_match(run + 6, run + 10)
                    ],
                    "{case_mode:?} {overlapping}"
                );
            }
        }
    }

    #[test]
    fn folded_windows_find_matches_across_window_seams() {
        // A period coprime to the window length puts matches at varying seam offsets.
        let haystack = b"xAbAbA x aBa.".repeat(4 * FOLDED_WINDOW_LEN / 13 + 10);
        let engine = SinglePatternEngine::new(b"aba".to_vec());
        for whole_word in [false, true] {
            for overlapping in [false, true] {
                let config = SearchConfig::builder()
                    .case_mode(CaseMode::AsciiInsensitive)
                    .whole_word(whole_word)
                    .overlapping(overlapping)
                    .build();
                let collect = |strategy, limit: usize| {
                    let mut matches = Vec::new();
                    engine.visit_matches::<true>(&haystack, config, strategy, |matched| {
                        matches.push(matched);
                        matches.len() < limit
                    });
                    matches
                };

                let windowed = collect(profiles::SingleStrategy::AsciiFoldedMemmem, usize::MAX);
                let stopped = collect(profiles::SingleStrategy::AsciiFoldedMemmem, 2);

                // The anchored scan folds nothing, so it serves as the reference.
                let expected = collect(profiles::SingleStrategy::AsciiAnchored, usize::MAX);
                assert!(expected.len() > 2);
                assert_eq!(windowed, expected, "{whole_word} {overlapping}");
                assert_eq!(stopped, expected[..2]);
            }
        }
    }
}
