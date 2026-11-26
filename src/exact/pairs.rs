//! Line scanner for a few literal patterns.
//!
//! Each pattern contributes a *pair*: two of its bytes a fixed distance apart, chosen per
//! haystack from a sample so they are rare in that text. The scanner reads two streams
//! `distance` bytes apart and tests 64 positions at a time in auto-vectorized loops. Every
//! pair's *key* byte is distinct, so with several pairs the key selects the expected value
//! byte: one compare and one select per pair.
//!
//! Comparing and verifying alternate per batch of blocks. The compare loop is branch-free;
//! verification checks a pattern's head as one masked integer and decides whether two
//! occurrences share a line by looking for a line feed between them.

use std::{mem, ops::Range};

use memchr::{memchr, memrchr};

use crate::{
    CaseMode, SearchConfig, Watched, Watcher, fold_ascii, is_word_boundary, normalize_pattern,
    window_matches_ascii_insensitive,
};

mod plan;

pub(crate) use plan::Extent;

/// Positions tested per step.
pub(crate) const BLOCK: usize = 64;
/// Whether scans are weighed against packed search by the cost model in [`plan`], whose
/// estimates hold for aarch64 (one-instruction selects). Elsewhere only scans needing no
/// weighing are used.
const WEIGHS_SCANS: bool = cfg!(target_arch = "aarch64");
/// Largest pattern set scanned; each pair adds a compare and a select per vector.
const MAX_PATTERNS: usize = if WEIGHS_SCANS { 8 } else { 4 };
/// Largest distance between the bytes of a pair, shared by all pairs of a plan.
const MAX_DISTANCE: usize = 3;
/// Rarest offsets per pattern considered as one end of its pair.
const RARE_OFFSETS: usize = 3;
/// Bytes per sampled window: a few lines of typical text.
pub(super) const SAMPLE_WINDOW_LEN: usize = 256;
/// Positions per window where pairs are counted, so the second byte stays in the window.
const COUNTED_LEN: usize = 240;
/// Positions counted per step, few enough for a byte-sized count.
pub(super) const COUNTED_CHUNK_LEN: usize = 80;
/// Bytes of haystack (or of one thread's share) per sampled window.
const WINDOW_SPACING: usize = 256 * 1024;
/// Fewest windows sampled; fewer cannot tell a rare pair from a frequent one.
const MIN_SAMPLE_WINDOWS: usize = 4;
/// Most windows sampled, keeping the sampling cost fixed on long haystacks.
pub(super) const MAX_SAMPLE_WINDOWS: usize = 16;
/// Shortest haystack whose scan repays sampling it.
const MIN_HAYSTACK_LEN: usize = 128 * 1024;
/// Fewest sampled bytes per pair occurrence for a scan: one candidate per block.
const MIN_CANDIDATE_SPACING: usize = BLOCK;
/// Sampled bytes per rival byte beyond which the rival search stops too seldom to lose.
const RIVAL_RARE_SPACING: usize = 8 * BLOCK;
/// Sampled bytes per key occurrence beyond which a one-pair scan pre-filters blocks on the
/// key byte alone.
const KEY_ALONE_SPACING: usize = 4 * BLOCK;
/// Leading pattern bytes compared as one `u128`.
pub(super) const HEAD_LEN: usize = 16;
/// Blocks compared before verifying: enough to amortize the switch, few enough for a block
/// index to fit a byte and the flags to stay in L1.
pub(super) const BATCH: usize = 128;
/// Rejected candidates allowed before any byte is scanned.
pub(super) const REJECTED_ALLOWANCE: usize = 64;
/// Scanned bytes per allowed rejected candidate. Half the spacing of any one byte in
/// uniform random bytes, so a plan's `absent` byte does not end such a scan.
const REJECTED_SPACING: usize = 2 * BLOCK;

pub(super) type SampleWindow = [u8; SAMPLE_WINDOW_LEN];

// Compile-time checks on the size relations the sampling, counting and batching rely on.
const _: () = assert!(
    COUNTED_LEN + MAX_DISTANCE <= SAMPLE_WINDOW_LEN
        && COUNTED_LEN.is_multiple_of(COUNTED_CHUNK_LEN)
        && COUNTED_CHUNK_LEN <= u8::MAX as usize
        && MIN_SAMPLE_WINDOWS >= 2
        && SAMPLE_WINDOW_LEN.is_multiple_of(4)
        && BATCH <= u8::MAX as usize
        && MARK_BATCH <= u8::MAX as usize
        && BLOCK < u8::MAX as usize
        && MAX_PATTERNS <= u8::BITS as usize
        && align_of::<Flags>() == BLOCK
);

/// Scanner for lines holding any of up to [`MAX_PATTERNS`] literal patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairScanner {
    patterns: Vec<Literal>,
    shortest: usize,
    longest: usize,
    insensitive: bool,
    /// Whether occurrences must be bounded by non-word bytes.
    whole_word: bool,
    /// A byte the caller's own search stops at; the scanner declines haystacks where that
    /// search would be faster.
    rival: Option<u8>,
}

/// A pattern with its head packed for comparison in one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Literal {
    /// The pattern, ASCII-folded when matching insensitively.
    pub(super) bytes: Vec<u8>,
    /// The first [`HEAD_LEN`] bytes, little-endian, zero-padded.
    head: u128,
    /// Bits each haystack byte must share with `head`: all, all but bit 5 for a folded
    /// letter, or none past the pattern.
    mask: u128,
}

impl Literal {
    /// Packs `pattern`, folded as `case_mode` matches it.
    pub(crate) fn new(pattern: &[u8], case_mode: CaseMode) -> Self {
        let bytes = normalize_pattern(pattern, case_mode);
        let mut head = [0_u8; HEAD_LEN];
        let mut mask = [0_u8; HEAD_LEN];
        for ((head_byte, mask_byte), &byte) in head.iter_mut().zip(&mut mask).zip(&bytes) {
            *head_byte = byte;
            // Folded patterns hold lowercase letters only; the two cases differ in bit 5.
            let either_case = case_mode == CaseMode::AsciiInsensitive && byte.is_ascii_lowercase();
            *mask_byte = if either_case { !0x20 } else { u8::MAX };
        }
        Self {
            bytes,
            head: u128::from_le_bytes(head),
            mask: u128::from_le_bytes(mask),
        }
    }

    /// Whether the pattern occurs at `start`. Always `false` if fewer than [`HEAD_LEN`]
    /// bytes follow `start`.
    #[inline(always)]
    pub(crate) fn occurs_at(&self, haystack: &[u8], start: usize, insensitive: bool) -> bool {
        let Some(window) = haystack
            .get(start..)
            .and_then(<[u8]>::first_chunk::<HEAD_LEN>)
        else {
            return false;
        };
        // Branch-free compare of the whole head.
        let head_occurs = (u128::from_le_bytes(*window) ^ self.head) & self.mask == 0;
        head_occurs
            && (self.bytes.len() <= HEAD_LEN || self.tail_occurs(haystack, start, insensitive))
    }

    /// Whether the pattern's bytes past its head occur after a head at `start`. Cold and out
    /// of line so the verification loop keeps its registers.
    #[cold]
    #[inline(never)]
    fn tail_occurs(&self, haystack: &[u8], start: usize, insensitive: bool) -> bool {
        let tail = &self.bytes[HEAD_LEN..];
        haystack
            .get(start + HEAD_LEN..start + self.bytes.len())
            .is_some_and(|window| {
                if insensitive {
                    window_matches_ascii_insensitive(window, tail)
                } else {
                    window == tail
                }
            })
    }
}

/// One end of a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairEnd {
    First,
    Second,
}

impl PairEnd {
    /// Index of this end in a pair stored in haystack order.
    fn index(self) -> usize {
        match self {
            Self::First => 0,
            Self::Second => 1,
        }
    }
}

/// The pairs chosen for one haystack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairPlan {
    /// Offset of each pair's second byte from its first.
    distance: usize,
    /// The distinct pairs as `[key, value]`, in the leading `pair_count` entries.
    pairs: [[u8; 2]; MAX_PATTERNS],
    pair_count: usize,
    /// Whether keys are the second byte of each pair in haystack order.
    keyed_on_second: bool,
    /// The value expected where no key lies: a byte the haystack rarely (or, when folding,
    /// never) holds.
    absent: u8,
    /// Per key byte, a bit for each pattern whose pair has that key.
    owners: [u8; 256],
    /// Per pattern, the offset of its pair's first byte.
    anchors: [usize; MAX_PATTERNS],
    /// Whether haystack letters are compared in either case.
    fold: bool,
    /// Whether blocks are tested for the key alone before both streams are compared.
    key_alone: bool,
}

impl PairPlan {
    /// A plan for each pattern's pair at its `anchors` offset, bytes `distance` apart. Returns
    /// `None` if neither end of the pairs is distinct enough to be the key. `rarest` is the
    /// haystack's least frequent byte.
    fn new(
        scanner: &PairScanner,
        distance: usize,
        anchors: [usize; MAX_PATTERNS],
        rarest: u8,
    ) -> Option<Self> {
        let pattern_pairs = scanner
            .patterns
            .iter()
            .zip(&anchors)
            .map(|(pattern, &anchor)| [pattern.bytes[anchor], pattern.bytes[anchor + distance]]);
        // Insensitive patterns are lowercase, so fold only if some pair byte is a letter.
        let fold = scanner.insensitive
            && pattern_pairs
                .clone()
                .flatten()
                .any(|byte| byte.is_ascii_lowercase());
        // Folding sets bit 5 on both sides; extra non-letter candidates fail verification.
        let fold_bit = if fold { 0x20 } else { 0 };
        let pattern_pairs = pattern_pairs.map(|pair| pair.map(|byte| byte | fold_bit));
        let mut plan = Self {
            distance,
            pairs: [[0; 2]; MAX_PATTERNS],
            pair_count: 0,
            keyed_on_second: false,
            // Folded haystack bytes have bit 5 set, so none equals zero.
            absent: if fold { 0 } else { rarest },
            owners: [0; 256],
            anchors,
            fold,
            key_alone: false,
        };
        for pair in pattern_pairs.clone() {
            if !plan.pairs[..plan.pair_count].contains(&pair) {
                plan.pairs[plan.pair_count] = pair;
                plan.pair_count += 1;
            }
        }
        let pairs = &mut plan.pairs[..plan.pair_count];
        let distinct_at = |end: PairEnd| {
            let end = end.index();
            pairs
                .iter()
                .enumerate()
                .all(|(index, pair)| pairs[..index].iter().all(|other| other[end] != pair[end]))
        };
        if !distinct_at(PairEnd::First) {
            if !distinct_at(PairEnd::Second) {
                return None;
            }
            plan.keyed_on_second = true;
            pairs.iter_mut().for_each(|pair| pair.reverse());
        }
        for (pattern, pair) in pattern_pairs.enumerate() {
            let key = pair[usize::from(plan.keyed_on_second)];
            plan.owners[usize::from(key)] |= 1 << pattern;
        }
        Some(plan)
    }

    /// For a one-pair plan, keys on the pair's rarer byte in the sample and enables
    /// `key_alone` if it is rare enough. Plans of several pairs need their keys as is.
    fn key_on_rarer_byte(&mut self, byte_counts: &[u16; 256], sample_len: usize) {
        if self.pair_count != 1 {
            return;
        }
        let occurrences = |byte: u8| {
            let count = |byte: u8| usize::from(byte_counts[usize::from(byte)]);
            // With folding, the byte without bit 5 is flagged too.
            count(byte) + if self.fold { count(byte & !0x20) } else { 0 }
        };
        let [key, value] = self.pairs[0];
        let (key, value) = if occurrences(value) < occurrences(key) {
            self.keyed_on_second = !self.keyed_on_second;
            self.owners[usize::from(value)] = std::mem::take(&mut self.owners[usize::from(key)]);
            (value, key)
        } else {
            (key, value)
        };
        self.pairs[0] = [key, value];
        self.key_alone = occurrences(key) * KEY_ALONE_SPACING <= sample_len;
    }

    /// Leading positions a scan compares: whole blocks of both streams, each followed by a
    /// full head for verification.
    fn scannable_len(&self, haystack: &[u8]) -> usize {
        haystack.len().saturating_sub(self.distance + HEAD_LEN) / BLOCK * BLOCK
    }

    /// Offset of the key stream into the haystack.
    fn key_offset(&self) -> usize {
        if self.keyed_on_second {
            self.distance
        } else {
            0
        }
    }

    /// Whether the kernel also flags keyless positions holding `absent` as their value.
    fn flags_absent(&self) -> bool {
        self.pair_count > 1 && !self.fold
    }
}

/// A value that aligns the stack frame holding it to a page, of the largest common size.
///
/// The frames of the functions called from that frame then start at a page boundary and,
/// while they are smaller than a page, hold none, so nothing they spill straddles two pages.
/// Reading a straddling spill back stalls, and the loop that verifies occurrences spills what
/// it carries from one occurrence to the next: at 2 of every 1024 stack positions it ran
/// three times slower.
#[derive(Debug)]
#[repr(align(16384))]
struct PageAligned<T>(T);

/// One block's flags, cache-line aligned so the per-block store never straddles two lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(align(64))]
pub(super) struct Flags(pub(super) [u8; BLOCK]);

/// The blocks of a batch of up to `SLOTS` blocks (default [`BATCH`]) that hold a flagged
/// position.
#[derive(Debug)]
pub(super) struct Hits<const SLOTS: usize = BATCH> {
    /// Flags of the flagged blocks, in the leading `count` entries.
    pub(super) flags: [Flags; SLOTS],
    /// Index within the batch of each flagged block.
    pub(super) blocks: [u8; SLOTS],
    pub(super) count: usize,
}

impl<const SLOTS: usize> Hits<SLOTS> {
    pub(super) fn empty() -> Self {
        Self {
            flags: [Flags([0; BLOCK]); SLOTS],
            blocks: [0; SLOTS],
            count: 0,
        }
    }

    /// Writes a block's flags to slot `kept` and returns the new kept count, one more if
    /// `any` (the OR of the flags) is nonzero. Branch-free, to avoid mispredicting on hits.
    #[inline(always)]
    pub(super) fn record(&mut self, kept: usize, block: u8, flags: [u8; BLOCK], any: u8) -> usize {
        // `kept` never exceeds the block index; the remainder elides a bounds check.
        self.flags[kept % SLOTS] = Flags(flags);
        self.blocks[kept % SLOTS] = block;
        kept + usize::from(any != 0)
    }

    /// Each flagged block's index in the batch with a bitmask of its flagged positions.
    #[inline(always)]
    pub(super) fn flagged(&self) -> impl Iterator<Item = (usize, u64)> {
        let blocks = self.flags.iter().zip(&self.blocks).take(self.count);
        blocks.map(|(flags, &block)| {
            // Most blocks hold one candidate, which two reductions find faster than packing.
            let (first_flagged, last_flagged) = flagged_range(&flags.0);
            let candidates = if first_flagged == last_flagged {
                1 << first_flagged
            } else {
                flag_bits(&flags.0)
            };
            (usize::from(block), candidates)
        })
    }
}

/// Offsets of the `COUNT` rarest bytes of `pattern` by sample counts, rarest (then earliest)
/// first, padded with `usize::MAX`.
pub(super) fn rare_offsets<const COUNT: usize>(
    pattern: &[u8],
    byte_counts: &[u16; 256],
) -> [usize; COUNT] {
    let mut rare = [usize::MAX; COUNT];
    for rank in 0..COUNT.min(pattern.len()) {
        rare[rank] = (0..pattern.len())
            .filter(|offset| !rare[..rank].contains(offset))
            .min_by_key(|&offset| byte_counts[usize::from(pattern[offset])])
            .expect("fewer offsets are taken than the pattern has");
    }
    rare
}

/// How far a scan of a haystack went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scanned {
    /// Matching lines counted.
    pub(crate) count: usize,
    /// Where the caller must count matching lines from: the haystack length, a line start,
    /// or a point in an uncounted line with no earlier-starting occurrence. Meaningless once
    /// a NUL byte was watched.
    pub(crate) unscanned: usize,
    /// What the scan watched of the haystack's leading bytes: see
    /// [`PairScanner::visit_lines`]. A scan that finds a NUL byte ends there, so the bytes
    /// may hold more.
    pub(crate) watched: Watched,
}

/// What a scan has found so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tally {
    /// Lines counted.
    count: usize,
    /// Candidates that failed verification.
    rejected: usize,
    /// A position in the last counted line (or its line feed), at or after the last
    /// occurrence's pair and before every candidate still to verify.
    last: Option<usize>,
}

/// Flags positions where some pair has its key in `keys` and its value in `values`, and
/// returns the flags with their OR (accumulated here because testing afterwards costs more).
///
/// With several pairs, the key selects the one expected value, so each pair costs a compare
/// and a select. Keyless positions expect `absent` and so are flagged if they hold it; a
/// single pair is compared directly. With `FOLD`, bit 5 is set in both streams first.
#[inline(always)]
fn pair_flags<const PAIRS: usize, const FOLD: bool>(
    pairs: &[[u8; 2]; PAIRS],
    absent: u8,
    keys: &[u8; BLOCK],
    values: &[u8; BLOCK],
) -> ([u8; BLOCK], u8) {
    let mut flags = [0_u8; BLOCK];
    let mut any = 0_u8;
    for ((flag, &key), &value) in flags.iter_mut().zip(keys).zip(values) {
        let (key, value) = if FOLD {
            (key | 0x20, value | 0x20)
        } else {
            (key, value)
        };
        // `PAIRS` is a compile-time constant, so each copy keeps only one arm.
        let hit = match pairs.as_slice() {
            // Non-short-circuiting `&` keeps the loop branch-free.
            [pair] => (key == pair[0]) & (value == pair[1]),
            pairs => {
                let mut expected = absent;
                for pair in pairs {
                    expected = if key == pair[0] { pair[1] } else { expected };
                }
                value == expected
            }
        };
        *flag = if hit { u8::MAX } else { 0 };
        any |= *flag;
    }
    (flags, any)
}

/// Nonzero if some position of `keys` holds `key` (which has bit 5 set when `FOLD`).
#[inline(always)]
fn key_flag<const FOLD: bool>(key: u8, keys: &[u8; BLOCK]) -> u8 {
    keys.iter().fold(0, |any, &byte| {
        let byte = if FOLD { byte | 0x20 } else { byte };
        any | if byte == key { u8::MAX } else { 0 }
    })
}

/// The bytes that one step of a compare loop reads.
pub(crate) type Block = [u8; BLOCK];

/// Splits `bytes` into blocks of each position's byte and of the byte after it, for
/// comparing adjacent byte pairs.
///
/// Returns the whole first-byte blocks, the whole second-byte blocks, and one zero-padded
/// block of each for the remaining positions. A compare flagging a pair of zeros may flag
/// padding positions past the end.
pub(crate) fn adjacent_blocks(bytes: &[u8]) -> (&[Block], &[Block], [Block; 2]) {
    let positions = bytes.len().saturating_sub(1);
    let whole = positions / BLOCK * BLOCK;
    let (firsts, _) = bytes[..whole].as_chunks::<BLOCK>();
    let (seconds, _) = bytes
        .get(1..=whole)
        .unwrap_or_default()
        .as_chunks::<BLOCK>();
    let rest = positions - whole;
    let mut last = [[0; BLOCK]; 2];
    last[0][..rest].copy_from_slice(&bytes[whole..positions]);
    last[1][..rest].copy_from_slice(&bytes[bytes.len() - rest..]);
    (firsts, seconds, last)
}

/// The indexes of the set bits of a mask, lowest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetBits(u64);

impl Iterator for SetBits {
    type Item = usize;

    #[inline(always)]
    fn next(&mut self) -> Option<usize> {
        if self.0 == 0 {
            return None;
        }
        let index = self.0.trailing_zeros() as usize;
        // Clears the lowest set bit.
        self.0 &= self.0 - 1;
        Some(index)
    }
}

/// The flagged positions of a block (flags are `0` or `u8::MAX`), lowest first.
#[inline(always)]
pub(crate) fn flagged_positions(flags: &[u8; BLOCK]) -> SetBits {
    SetBits(flag_bits(flags))
}

/// Blocks [`for_each_mark`] compares before visiting; small so short inputs stay cheap.
const MARK_BATCH: usize = 16;

/// Whether the first bytes of `marks` all differ once the bits of `set_bits` are set in them.
fn first_bytes_differ<const MARKS: usize>(marks: &[[u8; 2]; MARKS], set_bits: u8) -> bool {
    (0..MARKS).all(|mark| {
        marks[..mark]
            .iter()
            .all(|other| other[0] | set_bits != marks[mark][0] | set_bits)
    })
}

/// Whether [`for_each_mark`] can find `marks` whatever the case of the bytes it scans: their
/// first bytes still differ once the compare sets the bit that tells ASCII case apart.
pub(crate) fn marks_differ_folded<const MARKS: usize>(marks: &[[u8; 2]; MARKS]) -> bool {
    first_bytes_differ(marks, 0x20)
}

/// Calls `visit(position, mark_index)` for every position of `bytes` where one of `marks`
/// starts, in ascending order, using the line counter's branch-free batched compare loop.
///
/// The first bytes of `marks` must all differ. With `FOLD`, a mark also starts where
/// `bytes` holds it in another ASCII case; the marks must then hold no uppercase letter and
/// satisfy [`marks_differ_folded`].
///
/// # Examples
///
/// ```ignore
/// let mut found = Vec::new();
/// for_each_mark::<2, false>(b"a[[b]] ''", &[*b"[[", *b"''"], |position, mark| {
///     found.push((position, mark));
/// });
/// assert_eq!(found, vec![(1, 0), (7, 1)]);
/// ```
pub(crate) fn for_each_mark<const MARKS: usize, const FOLD: bool>(
    bytes: &[u8],
    marks: &[[u8; 2]; MARKS],
    mut visit: impl FnMut(usize, usize),
) {
    debug_assert!(
        first_bytes_differ(marks, 0),
        "the first byte of a mark selects its second"
    );
    debug_assert!(
        !FOLD || marks_differ_folded(marks),
        "the first byte of a mark selects its second in either case"
    );
    // The folding compare sets bit 5 of every byte, so it never sees a zero byte.
    let (compared, absent) = if FOLD {
        (marks.map(|mark| mark.map(|byte| byte | 0x20)), 0)
    } else {
        (*marks, u8::MAX)
    };
    let (firsts, seconds, [last_firsts, last_seconds]) = adjacent_blocks(bytes);
    // The padded last block is a batch of its own (`from_ref` makes a one-element slice), so
    // `visit` has a single call site and inlines into the loop.
    let last = (
        std::slice::from_ref(&last_firsts),
        std::slice::from_ref(&last_seconds),
    );
    let mut whole = firsts.chunks(MARK_BATCH).zip(seconds.chunks(MARK_BATCH));
    let mut hits = Hits::<MARK_BATCH>::empty();
    let mut batch_start = 0;
    // A manual loop rather than `Iterator::chain`, which compiles the compare loop worse.
    loop {
        let batch = whole.next();
        let (firsts, seconds) = batch.unwrap_or(last);
        let mut kept = 0;
        for (block, (firsts, seconds)) in (0_u8..).zip(firsts.iter().zip(seconds)) {
            let (flags, any) = pair_flags::<MARKS, FOLD>(&compared, absent, firsts, seconds);
            kept = hits.record(kept, block, flags, any);
        }
        hits.count = kept;
        for (block, flagged) in hits.flagged() {
            for offset in SetBits(flagged) {
                let position = batch_start + block * BLOCK + offset;
                // Rejects padded positions past the end, keyless positions flagged for
                // holding `0xFF`, and bytes that only match with bit 5 set. `if let ... &&
                // let ...` requires both patterns to match.
                if let Some(pair) = bytes.get(position..).and_then(<[u8]>::first_chunk::<2>)
                    && let pair = if FOLD { pair.map(fold_ascii) } else { *pair }
                    && let Some(mark) = marks.iter().position(|mark| *mark == pair)
                {
                    visit(position, mark);
                }
            }
        }
        if batch.is_none() {
            break;
        }
        batch_start += firsts.len() * BLOCK;
    }
}

/// Packs `flags`, each `0` or `u8::MAX`, into a bitmask.
#[inline(always)]
pub(super) fn flag_bits(flags: &[u8; BLOCK]) -> u64 {
    let (words, _) = flags.as_chunks::<8>();
    // The multiply gathers bit 0 of each of a word's eight bytes into its top byte.
    words.iter().rev().fold(0_u64, |mask, word| {
        let bits = u64::from_le_bytes(*word) & 0x0101_0101_0101_0101;
        (mask << 8) | (bits.wrapping_mul(0x0102_0408_1020_4080) >> 56)
    })
}

/// Indexes of the first and last flagged bytes of `flags`; at least one must be flagged.
#[inline(always)]
pub(super) fn flagged_range(flags: &[u8; BLOCK]) -> (u8, u8) {
    let mut first = u8::MAX;
    let mut last = 0;
    // Vector min/max over indexes, with clear bytes mapped to the losing value.
    for (index, &flag) in (0_u8..).zip(flags) {
        first = first.min(index | !flag);
    }
    for (index, &flag) in (0_u8..).zip(flags) {
        last = last.max(index & flag);
    }
    (first, last)
}

/// Whether `haystack[from..to]` holds a line feed, for `from <= to <= haystack.len()`.
#[inline(always)]
fn line_ends_between(haystack: &[u8], from: usize, to: usize) -> bool {
    debug_assert!(from <= to && to <= haystack.len());
    // Usually a line feed lies in the block before `to`; one vector max finds the last one,
    // which may lie before `from`.
    let Some(before) = haystack[..to].last_chunk::<BLOCK>() else {
        return line_ends_within(&haystack[from..to]);
    };
    let start = to - BLOCK;
    // One past the index of the block's last line feed, or zero without one.
    let mut past_feed = 0_u8;
    for (index, &byte) in (1_u8..).zip(before) {
        past_feed = past_feed.max(if byte == b'\n' { index } else { 0 });
    }
    if past_feed != 0 {
        return start + usize::from(past_feed) > from;
    }
    // Otherwise check the block after `from`, then the rest. A range shorter than a block
    // was covered by the block just tested.
    let Some(after) = haystack[from..to].first_chunk::<BLOCK>() else {
        return false;
    };
    // A fold without an early exit vectorizes, unlike `contains`.
    let feeds = after
        .iter()
        .fold(0, |any, &byte| any | u8::from(byte == b'\n'));
    // `get` is `None` if the two blocks overlap, and empty if they touch.
    feeds != 0
        || haystack
            .get(from + BLOCK..start)
            .is_some_and(line_ends_within)
}

/// If `position` is still in the last counted line (the one holding `last`), returns where
/// that line ends, past its line feed. Otherwise `None`.
fn counted_line_end(haystack: &[u8], last: Option<usize>, position: usize) -> Option<usize> {
    let last = last?;
    (!line_ends_between(haystack, last, position)).then(|| {
        let rest = &haystack[position..];
        position + memchr(b'\n', rest).map_or(rest.len(), |end| end + 1)
    })
}

/// Whether `bytes` hold a line feed. Cold: only the first block and long lines need it.
#[cold]
#[inline(never)]
fn line_ends_within(bytes: &[u8]) -> bool {
    memchr(b'\n', bytes).is_some()
}

/// Counts the first [`COUNTED_LEN`] positions of `window` holding `pair`, its bytes
/// `distance` apart.
fn pair_occurrences(window: &SampleWindow, distance: usize, pair: [u8; 2]) -> usize {
    let first = &window[..COUNTED_LEN];
    let second = &window[distance..][..COUNTED_LEN];
    let (first, _) = first.as_chunks::<COUNTED_CHUNK_LEN>();
    let (second, _) = second.as_chunks::<COUNTED_CHUNK_LEN>();
    first
        .iter()
        .zip(second)
        .map(|(first, second)| {
            // A chunk's count fits a byte, so it is summed in byte-wide vector lanes.
            let count: u8 = first
                .iter()
                .zip(second)
                // `u8::from` turns `true` into 1 and `false` into 0.
                .map(|(&first, &second)| u8::from((first == pair[0]) & (second == pair[1])))
                .sum();
            usize::from(count)
        })
        .sum()
}

/// Counts every byte value in `sample`.
pub(super) fn count_bytes(sample: &[SampleWindow]) -> [u16; 256] {
    // Four tables, so runs of equal bytes do not serialize on one counter.
    let mut tables = [[0_u16; 256]; 4];
    for window in sample {
        for bytes in window.as_chunks::<4>().0 {
            for (table, &byte) in tables.iter_mut().zip(bytes) {
                table[usize::from(byte)] += 1;
            }
        }
    }
    // Destructures the array into its first table and an array of the other three.
    let [mut counts, rest @ ..] = tables;
    for table in rest {
        for (count, more) in counts.iter_mut().zip(table) {
            *count += more;
        }
    }
    counts
}

/// Windows sampled for `share` bytes: one per [`WINDOW_SPACING`], clamped.
fn window_count(share: usize) -> usize {
    (share / WINDOW_SPACING).clamp(MIN_SAMPLE_WINDOWS, MAX_SAMPLE_WINDOWS)
}

/// Starts of `count` windows spread evenly over `len` bytes, from the start to the end.
fn window_starts(len: usize, count: usize) -> impl ExactSizeIterator<Item = usize> {
    let stride = (len - SAMPLE_WINDOW_LEN) / (count - 1);
    (0..count).map(move |index| index * stride)
}

/// Copies the windows sampled for `share` bytes into the front of `windows`, lowercased if
/// `insensitive`, and returns how many. `haystack` must be at least one window long.
fn sample_windows(
    haystack: &[u8],
    insensitive: bool,
    windows: &mut [SampleWindow; MAX_SAMPLE_WINDOWS],
    share: usize,
) -> usize {
    let count = window_count(share);
    let starts = window_starts(haystack.len(), count);
    for (window, start) in windows.iter_mut().zip(starts) {
        window.copy_from_slice(&haystack[start..][..SAMPLE_WINDOW_LEN]);
        if insensitive {
            window.make_ascii_lowercase();
        }
    }
    count
}

/// Windows spread over a haystack, used to count byte and pair frequencies before scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairSample {
    windows: [SampleWindow; MAX_SAMPLE_WINDOWS],
    window_count: usize,
}

impl PairSample {
    /// Samples `haystack`, lowercased if `insensitive`, or returns `None` if it is shorter
    /// than one window.
    pub(crate) fn new(haystack: &[u8], insensitive: bool) -> Option<Self> {
        Self::for_share(haystack, insensitive, haystack.len())
    }

    /// Like [`Self::new`], but with as many windows as a haystack of `share` bytes (one
    /// thread's part) gets, spread over all of `haystack`.
    fn for_share(haystack: &[u8], insensitive: bool, share: usize) -> Option<Self> {
        if haystack.len() < SAMPLE_WINDOW_LEN {
            return None;
        }
        let mut windows = [[0; SAMPLE_WINDOW_LEN]; MAX_SAMPLE_WINDOWS];
        let window_count = sample_windows(haystack, insensitive, &mut windows, share);
        Some(Self {
            windows,
            window_count,
        })
    }

    pub(super) fn windows(&self) -> &[SampleWindow] {
        &self.windows[..self.window_count]
    }

    /// How many positions [`Self::occurrences`] counts pairs at.
    pub(crate) fn counted_positions(&self) -> usize {
        self.window_count * COUNTED_LEN
    }

    /// Counts sampled positions holding the adjacent bytes `pair` (lowercase if the sample
    /// was lowercased).
    pub(crate) fn occurrences(&self, pair: [u8; 2]) -> usize {
        sampled_occurrences(self.windows(), 1, pair)
    }
}

/// Counts the positions in `sample` that hold `pair`, its bytes `distance` apart.
fn sampled_occurrences(sample: &[SampleWindow], distance: usize, pair: [u8; 2]) -> usize {
    sample
        .iter()
        .map(|window| pair_occurrences(window, distance, pair))
        .sum()
}

impl PairScanner {
    /// Builds a scanner for one to [`MAX_PATTERNS`] patterns of at least two bytes, none
    /// holding a line terminator, matching with `config`'s case and word rules, or returns
    /// `None`. `rival` is a byte the caller's own search stops at, lowercase when matching
    /// insensitively.
    pub(crate) fn new(
        patterns: &[Vec<u8>],
        config: SearchConfig,
        rival: Option<u8>,
    ) -> Option<Self> {
        let case_mode = config.case_mode();
        debug_assert!(
            patterns
                .iter()
                .flatten()
                .all(|byte| !matches!(byte, b'\n' | b'\r')),
            "patterns holding a line terminator are counted line by line"
        );
        let shortest = patterns.iter().map(Vec::len).min()?;
        let longest = patterns.iter().map(Vec::len).max()?;
        (patterns.len() <= MAX_PATTERNS && shortest >= 2).then(|| Self {
            patterns: patterns
                .iter()
                .map(|pattern| Literal::new(pattern, case_mode))
                .collect(),
            shortest,
            longest,
            insensitive: case_mode == CaseMode::AsciiInsensitive,
            whole_word: config.whole_word(),
            rival,
        })
    }

    /// Whether some pattern occurs with its pair's first byte at `position`. `key` is the
    /// folded byte at `position + plan.key_offset()`.
    #[inline(always)]
    fn occurs_around(&self, plan: &PairPlan, haystack: &[u8], position: usize, key: u8) -> bool {
        let mut owners = plan.owners[usize::from(key)];
        let mut occurs = false;
        while owners != 0 {
            let pattern = owners.trailing_zeros() as usize;
            // Clears the lowest set bit.
            owners &= owners - 1;
            // A start before the haystack wraps around and fails `occurs_at`. The remainder
            // elides a bounds check.
            let start = position.wrapping_sub(plan.anchors[pattern % MAX_PATTERNS]);
            let literal = &self.patterns[pattern];
            occurs |= literal.occurs_at(haystack, start, self.insensitive)
                && (!self.whole_word
                    || is_word_boundary(haystack, start, start + literal.bytes.len()));
        }
        occurs
    }

    /// [`Self::visit_lines`] without visiting lines or watching bytes.
    #[cfg(test)]
    pub(crate) fn count_lines(&self, plan: &PairPlan, haystack: &[u8]) -> (usize, usize) {
        let scanned = self.visit_lines(plan, haystack, &Watcher::default(), |_| {});
        (scanned.count, scanned.unscanned)
    }

    /// Counts matching lines in `haystack`, which starts at a line start, as far as the scan
    /// goes, passing `visit` an offset inside each counted line, in order.
    ///
    /// The scan stops short of the end (less than a block and a head) and gives up early if
    /// candidates keep failing verification: see [`Scanned`].
    ///
    /// The scan also watches the bytes it compares for what `watcher` says, a batch of blocks
    /// at a time, just after comparing the batch, while it is in the nearest cache. It ends
    /// at a batch holding a NUL byte. Line feeds are counted from the line last visited on,
    /// since whoever numbers the lines knows that line's number: the count covers the
    /// watched bytes from an offset in that line, or all of them before the first visit.
    /// When `visit` is passed a line, `watcher` holds what had been watched before the
    /// line's batch, which ends within a batch of blocks of the offset passed and may end a
    /// few bytes past it.
    pub(crate) fn visit_lines(
        &self,
        plan: &PairPlan,
        haystack: &[u8],
        watcher: &Watcher,
        visit: impl FnMut(usize),
    ) -> Scanned {
        // Compile-time check that the `cfg`-gated arms below match `MAX_PATTERNS`.
        const _: () = assert!(MAX_PATTERNS == if cfg!(target_arch = "aarch64") { 8 } else { 4 });
        // Each arm picks one monomorphized `scan`.
        match (plan.pair_count, plan.fold, plan.key_alone) {
            (1, false, true) => self.scan::<1, false, true>(plan, haystack, watcher, visit),
            (1, true, true) => self.scan::<1, true, true>(plan, haystack, watcher, visit),
            (1, false, false) => self.scan::<1, false, false>(plan, haystack, watcher, visit),
            (2, false, false) => self.scan::<2, false, false>(plan, haystack, watcher, visit),
            (3, false, false) => self.scan::<3, false, false>(plan, haystack, watcher, visit),
            (4, false, false) => self.scan::<4, false, false>(plan, haystack, watcher, visit),
            (1, true, false) => self.scan::<1, true, false>(plan, haystack, watcher, visit),
            (2, true, false) => self.scan::<2, true, false>(plan, haystack, watcher, visit),
            (3, true, false) => self.scan::<3, true, false>(plan, haystack, watcher, visit),
            (4, true, false) => self.scan::<4, true, false>(plan, haystack, watcher, visit),
            // `#[cfg]` drops these arms, and their scans, where plans hold at most four pairs.
            #[cfg(target_arch = "aarch64")]
            (5, false, false) => self.scan::<5, false, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (6, false, false) => self.scan::<6, false, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (7, false, false) => self.scan::<7, false, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (8, false, false) => self.scan::<8, false, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (5, true, false) => self.scan::<5, true, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (6, true, false) => self.scan::<6, true, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (7, true, false) => self.scan::<7, true, false>(plan, haystack, watcher, visit),
            #[cfg(target_arch = "aarch64")]
            (8, true, false) => self.scan::<8, true, false>(plan, haystack, watcher, visit),
            _ => unreachable!("plans hold one to MAX_PATTERNS pairs, a lone key only for one"),
        }
    }

    /// The scan behind [`Self::visit_lines`], compiled per pair count, fold and key-alone
    /// test so each copy is straight-line vector code.
    #[inline(never)]
    fn scan<const PAIRS: usize, const FOLD: bool, const KEY_ALONE: bool>(
        &self,
        plan: &PairPlan,
        haystack: &[u8],
        watcher: &Watcher,
        mut visit: impl FnMut(usize),
    ) -> Scanned {
        let pairs: &[[u8; 2]; PAIRS] = plan
            .pairs
            .first_chunk()
            .expect("a scan is instantiated for at most MAX_PATTERNS pairs");
        let scannable = plan.scannable_len(haystack);
        let stream = |offset: usize| {
            // Fails only for a haystack too short for any block.
            let bytes = haystack.get(offset..offset + scannable).unwrap_or_default();
            bytes.as_chunks::<BLOCK>().0
        };
        let key_offset = plan.key_offset();
        let (keys, values) = (stream(key_offset), stream(plan.distance - key_offset));
        let mut hits = Hits::empty();
        // Small, but it takes a page of stack: see `PageAligned`.
        let mut tally = PageAligned(Tally::default());
        let tally = &mut tally.0;
        let mut position = 0_usize;
        let mut misjudged = false;
        // The key stream's bytes are the ones watched, so what was watched ends `key_offset`
        // bytes past `position`.
        let watch = watcher.watch();
        let mut watched = Watched::default();
        let mut watched_to = key_offset;
        if watch.any() && scannable > 0 {
            watched.pass_to(haystack, 0..watched_to, watch, None);
            watcher.note(watched);
        }
        // Usually one pass. A pass ends early to skip the rest of a counted line, and the
        // next resumes in the block where that line ends.
        while position < scannable && !misjudged && !watched.nul {
            let first = position / BLOCK;
            let batches = keys[first..]
                .chunks(BATCH)
                .zip(values[first..].chunks(BATCH));
            for (keys, values) in batches {
                let mut count = 0_usize;
                let mut compare = |block: u8, keys: &[u8; BLOCK], values: &[u8; BLOCK]| {
                    let (flags, any) = pair_flags::<PAIRS, FOLD>(pairs, plan.absent, keys, values);
                    count = hits.record(count, block, flags, any);
                };
                if KEY_ALONE {
                    // Only blocks holding the key are compared in both streams.
                    let mut keyed = [0_u8; BATCH];
                    let mut keyed_count = 0_usize;
                    for (block, keys) in (0_u8..).zip(keys) {
                        keyed[keyed_count % BATCH] = block;
                        keyed_count += usize::from(key_flag::<FOLD>(pairs[0][0], keys) != 0);
                    }
                    for &block in &keyed[..keyed_count] {
                        let index = usize::from(block);
                        compare(block, &keys[index], &values[index]);
                    }
                } else {
                    for (block, (keys, values)) in (0_u8..).zip(keys.iter().zip(values)) {
                        compare(block, keys, values);
                    }
                }
                hits.count = count;
                let batch_end = position + keys.len() * BLOCK;
                // Computed before settling, so a skipped line raises the allowance only from
                // the next batch on.
                let allowed = REJECTED_ALLOWANCE + batch_end / REJECTED_SPACING;
                let last = tally.last;
                let line_end_block = self.settle(
                    plan,
                    haystack,
                    &hits,
                    (position..batch_end, &mut *tally),
                    &mut visit,
                );
                // Updated before any exit: `position` must not lie before `tally.last`.
                position = line_end_block.unwrap_or(batch_end);
                misjudged = tally.rejected > allowed;
                if watch.any() {
                    // The batch, with the rest of a skipped line, which no batch compares.
                    let found = tally.last.filter(|_| tally.last != last);
                    let batch = mem::replace(&mut watched_to, key_offset + position)..watched_to;
                    watched.pass_to(haystack, batch, watch, found);
                    watcher.note(watched);
                }
                if misjudged || line_end_block.is_some() || watched.nul {
                    break;
                }
            }
        }
        let Tally { count, last, .. } = *tally;
        let unscanned = counted_line_end(haystack, last, position).unwrap_or_else(|| {
            // The line at `position` is uncounted and every earlier pair was verified, so an
            // unseen occurrence starts at most a pattern length back, and not before the
            // line.
            let earliest = position.saturating_sub(self.longest - 1);
            memrchr(b'\n', &haystack[earliest..position]).map_or(earliest, |end| earliest + end + 1)
        });
        Scanned {
            count,
            unscanned,
            watched,
        }
    }

    /// Verifies one batch's candidates and adds their new lines to `tally`, passing an offset
    /// in each to `visit`.
    ///
    /// If the batch found occurrences but no new line, the line at its end was already
    /// counted, so this returns the block where that line ends (capped at the scannable
    /// length) for the scan to skip to. This keeps long lines with many matches cheap;
    /// other batches avoid finding line ends, which would slow the fastest scans.
    ///
    /// Kept out of line, shared by all compare loops, so neither competes for registers.
    #[inline(never)]
    fn settle(
        &self,
        plan: &PairPlan,
        haystack: &[u8],
        hits: &Hits,
        (batch, tally): (Range<usize>, &mut Tally),
        visit: &mut impl FnMut(usize),
    ) -> Option<usize> {
        let key_offset = plan.key_offset();
        let fold_bit = if plan.fold { 0x20 } else { 0 };
        // Copied out and written back once, so the loop keeps the results in registers.
        let Tally {
            mut count,
            mut rejected,
            mut last,
        } = *tally;
        for (block, mut candidates) in hits.flagged() {
            let block_start = batch.start + block * BLOCK;
            while candidates != 0 {
                let candidate = block_start + candidates.trailing_zeros() as usize;
                // Clears the lowest set bit.
                candidates &= candidates - 1;
                let key = haystack[candidate + key_offset] | fold_bit;
                if self.occurs_around(plan, haystack, candidate, key) {
                    // Only whether a line ends in between matters, not where.
                    let new_line =
                        last.is_none_or(|last| line_ends_between(haystack, last, candidate));
                    if new_line {
                        visit(candidate);
                    }
                    count += usize::from(new_line);
                    last = Some(candidate);
                } else {
                    rejected += 1;
                }
            }
        }
        let recounted = count == tally.count && last != tally.last;
        // `flatten` turns `Some(None)` (the line ended within the batch) into `None`.
        let line_end_block = recounted
            .then(|| counted_line_end(haystack, last, batch.end))
            .flatten()
            .map(|line_end| plan.scannable_len(haystack).min(line_end / BLOCK * BLOCK));
        if let Some(block) = line_end_block {
            // Still in the counted line (or its line feed), so later line-end tests start
            // here rather than far behind.
            last = Some(block - 1);
        }
        *tally = Tally {
            count,
            rejected,
            last,
        };
        line_end_block
    }
}

#[cfg(test)]
mod tests;
