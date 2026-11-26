//! Forward-moving needle search shared by the structured scanners' container pruning.

use std::cell::RefCell;

use memchr::memmem;

use crate::fold_ascii;

/// Bytes searched ahead of the scan position at once, bounding wasted work when the needle
/// is absent.
pub(crate) const SEARCH_WINDOW: usize = 64 * 1024;

/// Initial window of a folded search; windows double up to [`SEARCH_WINDOW`].
const FIRST_FOLDED_WINDOW: usize = 512;

thread_local! {
    /// Per-thread fold buffer, reused across searches.
    static FOLDED_WINDOW: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// A needle searched for as written or under ASCII case folding. Folded searches fold one
/// window at a time instead of copying the whole document.
#[derive(Debug)]
pub(crate) struct Needle {
    finder: memmem::Finder<'static>,
    folds: bool,
}

impl Needle {
    /// Builds a needle searcher. With `folds`, `needle` must already be folded.
    pub(crate) fn new(needle: &[u8], folds: bool) -> Self {
        debug_assert!(!folds || needle.iter().all(|&byte| fold_ascii(byte) == byte));
        Self {
            finder: memmem::Finder::new(needle).into_owned(),
            folds,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.finder.needle()
    }

    /// Returns the start of the first occurrence in `haystack`.
    pub(crate) fn find(&self, haystack: &[u8]) -> Option<usize> {
        if !self.folds {
            return self.finder.find(haystack);
        }
        let carry = self.as_bytes().len().saturating_sub(1);
        // At least twice the carried tail, so every window advances.
        let mut window_len = FIRST_FOLDED_WINDOW.max(2 * self.as_bytes().len());
        let mut start = 0;
        FOLDED_WINDOW.with_borrow_mut(|folded| {
            loop {
                let end = (start + window_len).min(haystack.len());
                folded.clear();
                folded.extend(haystack[start..end].iter().copied().map(fold_ascii));
                if let Some(offset) = self.finder.find(folded) {
                    return Some(start + offset);
                }
                if end == haystack.len() {
                    return None;
                }
                // Occurrences crossing `end` start within the carried tail.
                start = end - carry;
                window_len = (2 * window_len).min(SEARCH_WINDOW.max(window_len));
            }
        })
    }

    /// Iterates the starts of non-overlapping occurrences, like [`memmem::Finder::find_iter`].
    pub(crate) fn find_iter<'h>(&'h self, haystack: &'h [u8]) -> impl Iterator<Item = usize> + 'h {
        // An empty needle matches everywhere; step by at least one.
        let step = self.as_bytes().len().max(1);
        let mut position = 0;
        std::iter::from_fn(move || {
            let start = position + self.find(haystack.get(position..)?)?;
            position = start + step;
            Some(start)
        })
    }
}

/// What lies ahead of a scan position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextOccurrence {
    /// The first occurrence at or after the position ends here (exclusive).
    EndsAt(usize),
    /// No occurrence lies wholly between the position and here.
    NoneBefore(usize),
}

/// Answers "can this section be skipped?" for sections visited in document order, via
/// [`Self::skippable_end`]. Consecutive sections share one search until the scan passes the
/// found occurrence.
#[derive(Debug)]
pub(crate) struct OccurrenceCursor<'a> {
    needle: &'a Needle,
    bytes: &'a [u8],
    /// Start and result of the last search.
    last_search: Option<(usize, NextOccurrence)>,
}

impl<'a> OccurrenceCursor<'a> {
    pub(crate) fn new(needle: &'a Needle, bytes: &'a [u8]) -> Self {
        Self {
            needle,
            bytes,
            last_search: None,
        }
    }

    /// Finds what lies ahead of `start`, reusing the last search when it still applies.
    /// `start` must not decrease between calls.
    ///
    /// A section holds an occurrence iff it ends at or after `EndsAt(end)`. For sections
    /// ending past `NoneBefore(end)`, use [`Self::occurs_past`].
    fn next_from(&mut self, start: usize) -> NextOccurrence {
        let needle_len = self.needle.as_bytes().len();
        if let Some((searched_from, next)) = self.last_search
            && searched_from <= start
            && match next {
                NextOccurrence::EndsAt(end) => start + needle_len <= end,
                NextOccurrence::NoneBefore(end) => start < end,
            }
        {
            return next;
        }
        let window_end = start.saturating_add(SEARCH_WINDOW).min(self.bytes.len());
        let next = match self.needle.find(&self.bytes[start..window_end.max(start)]) {
            Some(offset) => NextOccurrence::EndsAt(start + offset + needle_len),
            None => NextOccurrence::NoneBefore(window_end),
        };
        self.last_search = Some((start, next));
        next
    }

    /// Whether an occurrence lies wholly inside `start..end`, given that none lies inside
    /// `start..searched_end`.
    fn occurs_past(&self, start: usize, searched_end: usize, end: usize) -> bool {
        // Occurrences straddling `searched_end` start up to a needle length before it.
        let from = (searched_end + 1)
            .saturating_sub(self.needle.as_bytes().len())
            .max(start);
        end > searched_end && self.needle.find(&self.bytes[from..end]).is_some()
    }

    /// Returns the end of the section starting at `start` if it holds no occurrence, so a
    /// scan can skip it.
    ///
    /// # Arguments
    ///
    /// * `start` - The section start; must not decrease between calls
    /// * `end_within` - Returns the section's end if it is at most the given limit, else
    ///   `None` (an unterminated section is taken to end at the end of the input)
    ///
    /// # Returns
    ///
    /// The section's end, or `None` if an occurrence lies wholly inside it
    pub(crate) fn skippable_end(
        &mut self,
        start: usize,
        end_within: impl FnOnce(usize) -> Option<usize>,
    ) -> Option<usize> {
        match self.next_from(start) {
            // A section ending before the occurrence's end can't hold it, so the walk for
            // its end can stop there.
            NextOccurrence::EndsAt(occurrence_end) if occurrence_end > start => {
                end_within(occurrence_end - 1)
            }
            // An empty needle occurs inside every section.
            NextOccurrence::EndsAt(_) => None,
            NextOccurrence::NoneBefore(searched_end) => {
                let input_len = self.bytes.len();
                let end = end_within(input_len).unwrap_or(input_len);
                (!self.occurs_past(start, searched_end, end)).then_some(end)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_answers_match_direct_section_searches() {
        // Adjacent occurrences, one straddling the first window's end, and one past an
        // empty window, to exercise every answer path.
        let mut document = vec![b'.'; 3 * SEARCH_WINDOW];
        for start in [10, 11, SEARCH_WINDOW - 1, 2 * SEARCH_WINDOW + 30_000] {
            document[start..start + 3].copy_from_slice(b"abc");
        }
        let finder = Needle::new(b"abc", false);
        let lens = [2, 4000, 70_000, 140_000];
        let sections = (0..document.len())
            .step_by(997)
            .flat_map(|start| lens.map(|len| (start, (start + len).min(document.len()))));
        let mut cursor = OccurrenceCursor::new(&finder, &document);
        let mut answered_from_tails = 0;

        for (start, end) in sections {
            let holds = match cursor.next_from(start) {
                NextOccurrence::EndsAt(occurrence_end) => occurrence_end <= end,
                NextOccurrence::NoneBefore(searched_end) => {
                    answered_from_tails += usize::from(end > searched_end);
                    cursor.occurs_past(start, searched_end, end)
                }
            };

            let expected = memmem::find(&document[start..end], b"abc").is_some();
            assert_eq!(holds, expected, "{start}..{end}");
        }
        assert!(answered_from_tails > 0);
    }

    #[test]
    fn tails_find_occurrences_straddling_or_past_the_window() {
        let mut document = vec![b'.'; 3 * SEARCH_WINDOW];
        let past_window = 2 * SEARCH_WINDOW + 500;
        document[SEARCH_WINDOW - 1..SEARCH_WINDOW + 2].copy_from_slice(b"abc");
        document[past_window..past_window + 3].copy_from_slice(b"abc");
        let finder = Needle::new(b"abc", false);
        let after_straddler = SEARCH_WINDOW + 2;
        let mut cursor = OccurrenceCursor::new(&finder, &document);

        let from_start = cursor.next_from(0);
        let straddling = cursor.occurs_past(0, SEARCH_WINDOW, SEARCH_WINDOW + 2);
        let cut_short = cursor.occurs_past(0, SEARCH_WINDOW, SEARCH_WINDOW + 1);
        // The earliest occurrence that a window misses ends one byte past it.
        let mut shifted = vec![b'.'; 2 * SEARCH_WINDOW];
        shifted[SEARCH_WINDOW - 2..SEARCH_WINDOW + 1].copy_from_slice(b"abc");
        let shifted_cursor = OccurrenceCursor::new(&finder, &shifted);
        let earliest_straddler = shifted_cursor.occurs_past(0, SEARCH_WINDOW, SEARCH_WINDOW + 1);
        let from_middle = cursor.next_from(after_straddler);
        let window_end = after_straddler + SEARCH_WINDOW;
        let beyond = cursor.occurs_past(after_straddler, window_end, past_window + 3);
        let short_of_it = cursor.occurs_past(after_straddler, window_end, past_window + 2);

        assert_eq!(from_start, NextOccurrence::NoneBefore(SEARCH_WINDOW));
        assert!(straddling);
        assert!(!cut_short);
        assert!(earliest_straddler);
        assert_eq!(from_middle, NextOccurrence::NoneBefore(window_end));
        assert!(beyond);
        assert!(!short_of_it);
    }

    #[test]
    fn empty_and_longer_than_window_needles_answer_at_the_edges() {
        // The only occurrence of the long needle starts at 0, before a section starting at 1.
        let mut document = vec![b'a'; SEARCH_WINDOW + 10];
        document[0] = b'b';
        let (empty, long) = (
            Needle::new(b"", false),
            Needle::new(&document[..SEARCH_WINDOW + 3], false),
        );

        let at_start = OccurrenceCursor::new(&empty, &document).next_from(7);
        let at_end = OccurrenceCursor::new(&empty, &document).next_from(document.len());
        let mut long_cursor = OccurrenceCursor::new(&long, &document);
        let first_window = long_cursor.next_from(0);
        let long_tail = long_cursor.occurs_past(0, SEARCH_WINDOW, document.len());
        let mut late_cursor = OccurrenceCursor::new(&long, &document);
        let late_window = late_cursor.next_from(1);
        let late_tail = late_cursor.occurs_past(1, SEARCH_WINDOW + 1, document.len());

        // An empty needle occurs at every position, including the end of the input.
        assert_eq!(at_start, NextOccurrence::EndsAt(7));
        assert_eq!(at_end, NextOccurrence::EndsAt(document.len()));
        assert_eq!(first_window, NextOccurrence::NoneBefore(SEARCH_WINDOW));
        assert!(long_tail);
        assert_eq!(late_window, NextOccurrence::NoneBefore(SEARCH_WINDOW + 1));
        assert!(!late_tail);
    }

    #[test]
    fn cursor_reuses_a_search_until_the_scan_passes_its_answer() {
        let document = b"..abc.....abc";
        let finder = Needle::new(b"abc", false);
        let mut cursor = OccurrenceCursor::new(&finder, document);

        let first = cursor.next_from(0);
        let reused = cursor.next_from(2);
        let search_after_reuse = cursor.last_search;
        let passed = cursor.next_from(3);
        let after_last = cursor.next_from(11);
        let reused_none = cursor.next_from(12);
        let search_after_none = cursor.last_search;

        assert_eq!(first, NextOccurrence::EndsAt(5));
        assert_eq!(reused, first);
        assert_eq!(search_after_reuse, Some((0, first)));
        assert_eq!(passed, NextOccurrence::EndsAt(13));
        assert_eq!(after_last, NextOccurrence::NoneBefore(document.len()));
        assert_eq!(reused_none, after_last);
        assert_eq!(search_after_none, Some((11, after_last)));
    }

    #[test]
    fn folded_needles_find_what_a_folded_copy_holds() {
        // Occurrences straddle window seams and the cap; the long needle exceeds the first
        // window.
        let mut document = vec![b'.'; 3 * SEARCH_WINDOW];
        for start in [
            FIRST_FOLDED_WINDOW - 1,
            3 * FIRST_FOLDED_WINDOW - 3,
            SEARCH_WINDOW - 2,
            2 * SEARCH_WINDOW + 5,
        ] {
            document[start..start + 3].copy_from_slice(b"AbC");
        }
        let long_needle = vec![b'q'; FIRST_FOLDED_WINDOW + 7];
        document[700..700 + long_needle.len()].fill(b'Q');
        let lowered = document.to_ascii_lowercase();

        for needle in [&b"abc"[..], b"", &long_needle] {
            let folded = Needle::new(needle, true);
            for start in [0, 1, 699, 701, 2000, SEARCH_WINDOW, document.len()] {
                let expected = memmem::find(&lowered[start..], needle);

                let found = folded.find(&document[start..]);

                assert_eq!(found, expected, "{} from {start}", needle.len());
            }
            assert_eq!(
                folded.find_iter(&document).collect::<Vec<_>>(),
                memmem::find_iter(&lowered, needle).collect::<Vec<_>>()
            );
            assert_eq!(
                Needle::new(needle, false).find(&document),
                memmem::find(&document, needle)
            );
        }
    }
}
