//! Counting line feeds and finding NUL bytes in one pass over bytes.
//!
//! A search that numbers lines or stops at NUL bytes would otherwise pass over its input once
//! for each. The loop here tallies both per position of the 64-byte blocks it reads, in
//! byte-wide lanes that the compiler keeps in vector registers: a compare and a subtract per
//! vector for line feeds, and one minimum per vector for NUL bytes.

use std::cell::Cell;
use std::ops::Range;

use memchr::{memchr, memchr_iter};

/// Bytes tallied per step.
const BLOCK: usize = 64;
/// Most blocks tallied before the lanes are added up: a lane counts one line feed per block.
const MAX_BLOCKS: usize = u8::MAX as usize;

/// What a pass over bytes looks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Watch {
    /// NUL bytes.
    pub(crate) nuls: bool,
    /// Line feeds, to count them.
    pub(crate) feeds: bool,
}

impl Watch {
    /// Whether a pass looks for anything.
    pub(crate) fn any(self) -> bool {
        self.nuls || self.feeds
    }
}

/// What is known of a haystack's leading bytes from having watched them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Watched {
    /// How many leading bytes the count of line feeds reaches.
    pub(crate) len: usize,
    /// The line feeds counted among them, from the last line a scan found if it found one.
    pub(crate) feeds: usize,
    /// Whether a NUL byte was found among them, or in the bytes a scan compared past them.
    pub(crate) nul: bool,
}

impl Watched {
    /// Watches `bytes`, which follow the bytes watched so far, for what `watch` says.
    pub(crate) fn pass(&mut self, bytes: &[u8], watch: Watch) {
        let (blocks, rest) = bytes.as_chunks::<BLOCK>();
        if watch.any() {
            for blocks in blocks.chunks(MAX_BLOCKS) {
                let (feeds, nul) = match (watch.nuls, watch.feeds) {
                    (true, true) => tally::<true, true>(blocks),
                    (true, false) => tally::<true, false>(blocks),
                    (false, _) => tally::<false, true>(blocks),
                };
                self.feeds += feeds;
                self.nul |= nul;
            }
        }
        if watch.feeds {
            self.feeds += memchr_iter(b'\n', rest).count();
        }
        self.nul |= watch.nuls && memchr(0, rest).is_some();
        self.len += bytes.len();
    }

    /// Watches `batch`, the bytes of `haystack` a scan has just compared, which follow the
    /// ones it had it watch before. With `found`, the offset of the last line the scan
    /// found in them, the count of line feeds starts over there and is left for a later
    /// call to make, as whoever was passed that line counts the lines before it and, where
    /// lines are found this often, the ones after it as well.
    ///
    /// Kept out of line, so the scans it serves stay small.
    #[inline(never)]
    pub(crate) fn pass_to(
        &mut self,
        haystack: &[u8],
        batch: Range<usize>,
        watch: Watch,
        found: Option<usize>,
    ) {
        match found {
            Some(found) if watch.feeds => {
                if watch.nuls {
                    self.pass(
                        &haystack[batch],
                        Watch {
                            feeds: false,
                            ..watch
                        },
                    );
                }
                *self = Self {
                    len: found,
                    feeds: 0,
                    nul: self.nul,
                };
            }
            Some(_) | None => {
                if watch.feeds {
                    self.feeds += memchr_iter(b'\n', &haystack[self.len..batch.start]).count();
                }
                self.len = batch.start;
                self.pass(&haystack[batch], watch);
            }
        }
    }
}

/// What a scan is to watch, with what it had watched before the bytes it is comparing.
///
/// Whoever is passed the lines a scan finds reads that here. It is not passed along with each
/// line: an argument more would cost the loop that verifies occurrences a register.
#[derive(Debug, Default)]
pub(crate) struct Watcher {
    watch: Watch,
    watched: Cell<Watched>,
}

impl Watcher {
    /// A watcher for `watch`, with nothing watched yet.
    #[cfg(any(feature = "cli", test))]
    pub(crate) fn new(watch: Watch) -> Self {
        Self {
            watch,
            watched: Cell::default(),
        }
    }

    /// What the scan is to watch.
    pub(crate) fn watch(&self) -> Watch {
        self.watch
    }

    /// What the scan last said it had watched.
    #[cfg(any(feature = "cli", test))]
    pub(crate) fn watched(&self) -> Watched {
        self.watched.get()
    }

    /// Takes what the scan has watched so far.
    pub(crate) fn note(&self, watched: Watched) {
        self.watched.set(watched);
    }
}

/// The line feeds of at most [`MAX_BLOCKS`] `blocks` if `FEEDS`, and whether they hold a NUL
/// byte if `NULS`.
///
/// Kept out of line: compiled alone, the lanes stay in registers, which they need not when
/// the loop is inlined into a caller with loops of its own.
#[inline(never)]
fn tally<const NULS: bool, const FEEDS: bool>(blocks: &[[u8; BLOCK]]) -> (usize, bool) {
    debug_assert!(blocks.len() <= MAX_BLOCKS);
    // Two sets of lanes, for alternate blocks, so a block's tallies need not wait for the
    // block before it.
    let mut feeds = [[0_u8; BLOCK]; 2];
    let mut lowest = [[u8::MAX; BLOCK]; 2];
    let mut add = |set: usize, block: &[u8; BLOCK]| {
        for ((feeds, lowest), &byte) in feeds[set].iter_mut().zip(&mut lowest[set]).zip(block) {
            if FEEDS {
                *feeds += u8::from(byte == b'\n');
            }
            if NULS {
                *lowest = (*lowest).min(byte);
            }
        }
    };
    let (pairs, odd) = blocks.as_chunks::<2>();
    for [even, odd] in pairs {
        add(0, even);
        add(1, odd);
    }
    for block in odd {
        add(0, block);
    }
    let feeds = feeds.as_flattened().iter().map(|&lane| usize::from(lane));
    let lowest = lowest.as_flattened().iter().copied().min();
    (feeds.sum(), lowest == Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Watch = Watch {
        nuls: true,
        feeds: true,
    };

    fn watched(bytes: &[u8], nuls: bool, feeds: bool) -> Watched {
        let mut watched = Watched::default();
        watched.pass(bytes, Watch { nuls, feeds });
        watched
    }

    #[test]
    fn nothing_but_line_feeds_are_counted_without_overflow() {
        let bytes = vec![b'\n'; (2 * MAX_BLOCKS + 1) * BLOCK + 7];

        let both = watched(&bytes, true, true);

        assert_eq!(
            both,
            Watched {
                len: bytes.len(),
                feeds: bytes.len(),
                nul: false
            }
        );
    }

    #[test]
    fn a_nul_byte_is_found_at_any_position_of_a_block_or_past_the_last() {
        let len = MAX_BLOCKS * BLOCK + 70;
        for at in [
            0,
            1,
            15,
            16,
            63,
            64,
            100,
            len - 71,
            len - 70,
            len - 6,
            len - 1,
        ] {
            let mut bytes = vec![b'a'; len];
            bytes[at] = 0;
            bytes[(at + 9) % len] = b'\n';

            let both = watched(&bytes, true, true);
            let feeds_only = watched(&bytes, false, true);
            let nuls_only = watched(&bytes, true, false);
            let neither = watched(&bytes, false, false);

            assert!(both.nul && nuls_only.nul && !feeds_only.nul, "NUL at {at}");
            assert_eq!((both.feeds, feeds_only.feeds, nuls_only.feeds), (1, 1, 0));
            assert_eq!(both.len, len);
            assert_eq!(
                neither,
                Watched {
                    len,
                    feeds: 0,
                    nul: false
                }
            );
        }
    }

    #[test]
    fn every_other_byte_is_neither_a_line_feed_nor_a_nul_byte() {
        let bytes: Vec<u8> = (1..=u8::MAX).filter(|&byte| byte != b'\n').collect();
        let bytes = bytes.repeat(3);

        assert_eq!(
            watched(&bytes, true, true),
            Watched {
                len: bytes.len(),
                feeds: 0,
                nul: false
            }
        );
    }

    fn feeds_in(text: &[u8], range: Range<usize>) -> usize {
        text[range].iter().filter(|&&byte| byte == b'\n').count()
    }

    #[test]
    fn a_count_starts_over_at_a_found_line_and_is_made_once_no_line_is_found() {
        let text = b"one\ntwo\n\nthree\n".repeat(700);
        let (first, found, second, end) = (1_000, 5_003, 9_001, 10_400);
        let mut watched = Watched::default();

        watched.pass_to(&text, 0..first, BOTH, None);
        let before = watched;
        watched.pass_to(&text, first..second, BOTH, Some(found));
        let at_found = watched;
        watched.pass_to(&text, second..end, BOTH, None);
        let mut nuls_only = before;
        let watch = Watch {
            feeds: false,
            ..BOTH
        };
        nuls_only.pass_to(&text, first..second, watch, Some(found));

        let counted = |len: usize, from: usize| Watched {
            len,
            feeds: feeds_in(&text, from..len),
            nul: false,
        };
        assert_eq!(before, counted(first, 0));
        assert_eq!(at_found, counted(found, found));
        assert_eq!(watched, counted(end, found));
        assert_eq!(
            nuls_only,
            Watched {
                len: second,
                ..before
            }
        );
    }

    #[test]
    fn a_nul_byte_is_found_in_a_batch_whether_or_not_a_line_was_found_in_it() {
        let mut text = b"one\ntwo\n\nthree\n".repeat(700);
        let (first, found, end) = (1_000, 5_003, 9_001);
        let feeds_only = Watch {
            nuls: false,
            ..BOTH
        };

        for nul_at in [first, found - 1, found + 1, end - 1] {
            text[nul_at] = 0;
            for line in [Some(found), None] {
                let mut both = Watched::default();
                both.pass_to(&text, 0..first, BOTH, None);
                let mut unwatched = both;
                both.pass_to(&text, first..end, BOTH, line);
                unwatched.pass_to(&text, first..end, feeds_only, line);

                assert!(both.nul && !unwatched.nul, "NUL at {nul_at}");
            }
            text[nul_at] = b'x';
        }
    }

    #[test]
    fn a_watcher_holds_what_was_last_noted() {
        let watcher = Watcher::new(BOTH);
        let noted = watched(b"one\ntwo\n", true, true);

        let before = watcher.watched();
        watcher.note(noted);

        assert_eq!(watcher.watch(), BOTH);
        assert_eq!(before, Watched::default());
        assert_eq!(watcher.watched(), noted);
        assert_eq!(Watcher::default().watch(), Watch::default());
    }

    #[test]
    fn passes_add_up_over_pieces_of_any_length() {
        let text = b"one\ntwo\n\nthree\n".repeat(700);
        let mut in_pieces = Watched::default();
        let mut rest = &text[..];
        for len in [0, 1, 63, 64, 65, 4096, 1000].into_iter().cycle() {
            let (piece, later) = rest.split_at(len.min(rest.len()));
            in_pieces.pass(piece, BOTH);
            rest = later;
            if rest.is_empty() {
                break;
            }
        }

        assert_eq!(in_pieces, watched(&text, true, true));
        assert_eq!(in_pieces.feeds, 4 * 700);
    }
}
