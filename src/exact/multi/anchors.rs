use aho_corasick::packed;
use smallvec::{SmallVec, smallvec};

use super::lines::count_packed_lines;
use crate::fold_ascii;

/// Haystack bytes sampled to estimate how often each pattern window occurs.
pub(super) const ANCHOR_SAMPLE_LEN: usize = 1024;
/// Smallest haystack for which choosing rare anchors repays sampling and building a searcher.
pub(super) const ANCHOR_MIN_HAYSTACK_LEN: usize = 128 * 1024;
/// Log2 of the number of buckets counting sampled windows.
const ANCHOR_HASH_BITS: u32 = 12;
/// Widest anchor: narrower anchors search faster, and three bytes stay selective.
const MAX_ANCHOR_LEN: usize = 3;

/// Packs the first (at most four) bytes of `window` into an integer.
#[inline(always)]
fn window_key(window: &[u8]) -> u32 {
    window
        .iter()
        .fold(0_u32, |key, &byte| (key << 8) | u32::from(byte))
}

#[inline(always)]
fn window_bucket(key: u32) -> usize {
    (key.wrapping_mul(0x9E37_79B1) >> (32 - ANCHOR_HASH_BITS)) as usize
}

/// Counts the `width`-byte windows of `sample` (ASCII-folded when `fold` is set) per hash
/// bucket. Colliding windows share a bucket, which only overestimates counts.
fn window_counts(sample: &[u8], width: usize, fold: bool) -> Vec<u16> {
    let mut counts = vec![0_u16; 1 << ANCHOR_HASH_BITS];
    let mask = u32::MAX >> (32 - 8 * width);
    let mut key = 0_u32;
    for (index, &byte) in sample.iter().enumerate() {
        let byte = if fold { fold_ascii(byte) } else { byte };
        key = ((key << 8) | u32::from(byte)) & mask;
        if index + 1 >= width {
            let count = &mut counts[window_bucket(key)];
            *count = count.saturating_add(1);
        }
    }
    counts
}

/// The patterns holding one anchor, each with the anchor's offset in it.
type AnchorHolders<'a> = SmallVec<[(&'a [u8], usize); 1]>;

/// Searches for one rare window (anchor) per pattern and verifies the pattern around each
/// hit. Beats prefix search when prefixes are common, like `stat` of `status=500` in logs.
pub(super) struct AnchoredSearcher<'a> {
    searcher: packed::Searcher,
    /// Indexed by the searcher's pattern IDs, which are the distinct anchors.
    pub(super) holders: Vec<AnchorHolders<'a>>,
}

impl<'a> AnchoredSearcher<'a> {
    /// Picks each pattern's rarest window in `sample`, preferring earlier windows on ties.
    /// Returns `None` when those anchors are not much rarer than the patterns' prefixes, so
    /// prefix search is about as fast.
    pub(super) fn new(patterns: &'a [Vec<u8>], sample: &[u8], fold: bool) -> Option<Self> {
        let anchor_len = patterns.iter().map(Vec::len).min()?.min(MAX_ANCHOR_LEN);
        let counts = window_counts(sample, anchor_len, fold);
        let count_of = |window: &[u8]| usize::from(counts[window_bucket(window_key(window))]);

        let mut prefix_total = 0_usize;
        let mut anchor_total = 0_usize;
        let mut anchors: Vec<&[u8]> = Vec::new();
        let mut holders: Vec<AnchorHolders<'_>> = Vec::new();
        for pattern in patterns {
            let (offset, anchor) = pattern
                .windows(anchor_len)
                .enumerate()
                .min_by_key(|(_, window)| count_of(window))?;
            prefix_total += count_of(&pattern[..anchor_len]);
            anchor_total += count_of(anchor);
            match anchors.iter().position(|&known| known == anchor) {
                Some(index) => holders[index].push((pattern, offset)),
                None => {
                    anchors.push(anchor);
                    holders.push(smallvec![(pattern.as_slice(), offset)]);
                }
            }
        }
        if anchor_total * 2 >= prefix_total || prefix_total < sample.len() / 256 {
            return None;
        }

        let mut config = packed::Config::new();
        config.match_kind(packed::MatchKind::LeftmostFirst);
        let mut builder = config.builder();
        builder.extend(anchors);
        let searcher = builder.build()?;
        Some(Self { searcher, holders })
    }

    /// Counts lines of `haystack` holding at least one pattern occurrence, passing `on_line`
    /// an offset in each. No pattern contains a newline, so an occurrence lies within the line
    /// of its anchor.
    pub(super) fn visit_lines(&self, haystack: &[u8], on_line: impl FnMut(usize)) -> usize {
        let verify = |hit: aho_corasick::Match, origin: usize| {
            self.holders[hit.pattern().as_usize()]
                .iter()
                .any(|&(pattern, offset)| {
                    (origin + hit.start())
                        .checked_sub(offset)
                        .and_then(|start| haystack.get(start..start + pattern.len()))
                        .is_some_and(|window| window == pattern)
                })
        };
        count_packed_lines(&self.searcher, haystack, verify, on_line)
    }
}
