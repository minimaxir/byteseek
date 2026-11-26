use std::ops::Range;

use memchr::memchr;
use smallvec::SmallVec;

use super::{ApproximateCandidate, ApproximatePattern, PrefilterContext};

/// Lane strides near a multiple of this alias in the cache, so they are shortened.
const CACHE_ALIAS_STRIDE: usize = 4096;
const CACHE_LINE_LEN: usize = 64;
/// Finds every window within one edit of any pattern in one bit-parallel pass (Shift-And
/// with one error, after Wu and Manber). Each pattern owns a run of bits in one `u64`; a
/// shift carries into the next pattern's first bit, which is always set anyway.
///
/// When the patterns use at most half the word, it holds several side-by-side copies, each
/// tracking a different haystack segment, all advanced by one state update.
pub(super) struct BitapPrefilter {
    /// Pattern bits whose byte equals the index byte.
    masks: [u64; 256],
    /// `masks` shifted into each copy, at index `byte * copies + copy`.
    packed_masks: Box<[u64]>,
    /// Copies per word: the largest power of two that fits in 64 bits.
    copies: usize,
    /// Bits in one copy of all patterns.
    pattern_bits: usize,
    first_bits: u64,
    accept_bits: u64,
    /// Owning pattern of each accept bit.
    pattern_by_bit: [u8; 64],
    pattern_lens: SmallVec<[usize; 8]>,
    longest_pattern: usize,
}

impl BitapPrefilter {
    /// Bytes per mark stop at which the mark pass costs the same as this one; denser marks
    /// favor this pass.
    ///
    /// A mark stop costs about twelve bytes of this pass with one copy. Packed copies share
    /// the state update, but table loads dominate from four copies on, capping the gain at
    /// about three times.
    pub(super) fn stop_spacing(&self) -> usize {
        12 * self.copies.min(3)
    }

    /// Builds the automaton if all patterns have halves and fit in 64 bits. With `folds`,
    /// the patterns' lowercase letters also match their uppercase in the haystack.
    pub(super) fn build(patterns: &[ApproximatePattern], folds: bool) -> Option<Box<Self>> {
        let pattern_bits = patterns
            .iter()
            .map(|pattern| pattern.bytes.len())
            .sum::<usize>();
        if !(1..=64).contains(&pattern_bits)
            || patterns.iter().any(|pattern| pattern.halves.is_none())
        {
            return None;
        }

        let copies = 1 << (64 / pattern_bits).ilog2();
        let mut prefilter = Box::new(Self {
            masks: [0; 256],
            packed_masks: Box::default(),
            copies,
            pattern_bits,
            first_bits: 0,
            accept_bits: 0,
            pattern_by_bit: [0; 64],
            pattern_lens: SmallVec::new(),
            longest_pattern: 0,
        });
        let mut bit = 0_usize;
        for (pattern_index, pattern) in patterns.iter().enumerate() {
            prefilter.first_bits |= 1 << bit;
            for &byte in &pattern.bytes {
                prefilter.masks[usize::from(byte)] |= 1 << bit;
                if folds {
                    prefilter.masks[usize::from(byte.to_ascii_uppercase())] |= 1 << bit;
                }
                bit += 1;
            }
            prefilter.accept_bits |= 1 << (bit - 1);
            prefilter.pattern_by_bit[bit - 1] =
                u8::try_from(pattern_index).expect("at most 16 patterns fit in 64 bits");
            prefilter.pattern_lens.push(pattern.bytes.len());
            prefilter.longest_pattern = prefilter.longest_pattern.max(pattern.bytes.len());
        }
        let mut packed_masks = Vec::with_capacity(256 * copies);
        for &mask in &prefilter.masks {
            packed_masks.extend((0..copies).map(|copy| mask << (copy * pattern_bits)));
        }
        prefilter.packed_masks = packed_masks.into_boxed_slice();
        Some(prefilter)
    }

    /// Returns windows starting in `owned` within one edit of a pattern, keeping only the
    /// best (lowest distance, then shortest) per pattern and end.
    ///
    /// Separate states per edit kind fix each window's length and distance at its end, so
    /// no verification is needed. Ordered by end, then pattern index.
    pub(super) fn region_windows(
        &self,
        ctx: &PrefilterContext<'_>,
        owned: Range<usize>,
    ) -> Vec<ApproximateCandidate> {
        let scan_end = ctx.haystack.len().min(owned.end + self.longest_pattern);
        let positions = owned.start..scan_end;
        let mut windows = Vec::new();
        // With fewer than four copies, interleaving words hides the serial update latency.
        match self.copies {
            ..=1 => self.lane_windows::<4, 1>(ctx, &owned, positions, &mut windows),
            2 | 3 => self.lane_windows::<2, 2>(ctx, &owned, positions, &mut windows),
            4..=7 => self.lane_windows::<1, 4>(ctx, &owned, positions, &mut windows),
            8..=15 => self.lane_windows::<1, 8>(ctx, &owned, positions, &mut windows),
            16.. => self.lane_windows::<1, 16>(ctx, &owned, positions, &mut windows),
        }
        windows
    }

    /// Appends, in end order, the windows of [`Self::region_windows`] ending in `positions`.
    ///
    /// `positions` splits into `WORDS * COPIES` equal lanes, each warmed up by replaying the
    /// `longest_pattern + 1` bytes before it; the remainder is scanned recursively. Const
    /// generics let the per-lane loops fully unroll.
    #[inline(never)]
    fn lane_windows<const WORDS: usize, const COPIES: usize>(
        &self,
        ctx: &PrefilterContext<'_>,
        owned: &Range<usize>,
        positions: Range<usize>,
        windows: &mut Vec<ApproximateCandidate>,
    ) {
        let haystack = ctx.haystack;
        let Some(lane_len) = self.lane_stride(positions.len(), WORDS * COPIES) else {
            let states = self.warmed_states(haystack, positions.start);
            self.scan_serial(ctx, owned, states, positions, windows);
            return;
        };

        let packed_masks = self.packed_mask_rows::<COPIES>();
        let first_bits = self.replicate(self.first_bits, COPIES);
        let accept_bits = self.replicate(self.accept_bits, COPIES);
        let lanes_end = positions.start + WORDS * COPIES * lane_len;
        let lane_starts: [[usize; COPIES]; WORDS] = std::array::from_fn(|word| {
            std::array::from_fn(|copy| positions.start + (word * COPIES + copy) * lane_len)
        });
        let mut lane_slices = haystack[positions.start..lanes_end].chunks_exact(lane_len);
        let lanes: [[&[u8]; COPIES]; WORDS] = std::array::from_fn(|_| {
            std::array::from_fn(|_| lane_slices.next().expect("one slice per lane"))
        });
        let mut states =
            self.warmed_packed_states(haystack, packed_masks, &lane_starts, first_bits);

        // Arrays of generic length don't implement `Default`.
        let mut per_lane: [[Vec<ApproximateCandidate>; COPIES]; WORDS] =
            std::array::from_fn(|_| std::array::from_fn(|_| Vec::new()));
        for offset in 0..lane_len {
            let mut accepted = 0;
            for (word_states, word_lanes) in states.iter_mut().zip(&lanes) {
                let mask = word_lanes.iter().enumerate().fold(0, |mask, (copy, lane)| {
                    mask | packed_masks[usize::from(lane[offset])][copy]
                });
                word_states.advance(mask, first_bits);
                accepted |= word_states.accepted(accept_bits);
            }
            if accepted == 0 {
                continue;
            }
            for ((word_states, starts), word_windows) in
                states.iter().zip(&lane_starts).zip(&mut per_lane)
            {
                if word_states.accepted(accept_bits) != 0 {
                    let copy_positions = starts.map(|start| start + offset);
                    self.emit_packed_windows(
                        ctx,
                        owned,
                        *word_states,
                        copy_positions,
                        word_windows,
                    );
                }
            }
        }
        // Lanes cover consecutive end ranges, so appending them keeps end order.
        windows.reserve(per_lane.iter().flatten().map(Vec::len).sum::<usize>());
        for lane in per_lane.into_iter().flatten() {
            windows.extend(lane);
        }
        self.lane_windows::<WORDS, COPIES>(ctx, owned, lanes_end..positions.end, windows);
    }

    /// Returns the lane length, or `None` if lanes would be too short to repay warming up.
    /// Strides near a multiple of `CACHE_ALIAS_STRIDE` are shortened to spread the lanes
    /// over distinct cache sets.
    fn lane_stride(&self, positions_len: usize, lanes: usize) -> Option<usize> {
        let lane_len = positions_len / lanes;
        if lane_len < 2 * (self.longest_pattern + 1) {
            return None;
        }
        let alias_offset = (lane_len + CACHE_LINE_LEN) % CACHE_ALIAS_STRIDE;
        if lane_len >= CACHE_ALIAS_STRIDE && alias_offset < 2 * CACHE_LINE_LEN {
            return Some(lane_len - alias_offset);
        }
        Some(lane_len)
    }

    /// Views `packed_masks` as one row of `COPIES` masks per byte value.
    fn packed_mask_rows<const COPIES: usize>(&self) -> &[[u64; COPIES]; 256] {
        self.packed_masks
            .as_chunks::<COPIES>()
            .0
            .try_into()
            .expect("dispatch packs exactly `self.copies` copies per row")
    }

    /// Returns packed states warmed up on the `longest_pattern + 1` bytes before each lane.
    /// Bytes before the haystack contribute no bits.
    fn warmed_packed_states<const WORDS: usize, const COPIES: usize>(
        &self,
        haystack: &[u8],
        packed_masks: &[[u64; COPIES]; 256],
        lane_starts: &[[usize; COPIES]; WORDS],
        first_bits: u64,
    ) -> [EditStates; WORDS] {
        let mut states = [EditStates::default(); WORDS];
        for distance in (1..=self.longest_pattern + 1).rev() {
            for (word_states, starts) in states.iter_mut().zip(lane_starts) {
                let mask = starts.iter().enumerate().fold(0, |mask, (copy, start)| {
                    start.checked_sub(distance).map_or(mask, |position| {
                        mask | packed_masks[usize::from(haystack[position])][copy]
                    })
                });
                word_states.advance(mask, first_bits);
            }
        }
        states
    }

    /// Emits the windows of every accepting copy in packed `states` into its own list.
    #[inline(always)]
    fn emit_packed_windows<const COPIES: usize>(
        &self,
        ctx: &PrefilterContext<'_>,
        owned: &Range<usize>,
        states: EditStates,
        positions: [usize; COPIES],
        windows: &mut [Vec<ApproximateCandidate>; COPIES],
    ) {
        let copy_mask = self.copy_mask();
        let accepted = states.accepted(self.replicate(self.accept_bits, COPIES));
        // An unrolled per-copy test avoids dividing bit indices by the copy width.
        for (copy, (&position, copy_windows)) in positions.iter().zip(windows).enumerate() {
            let shift = copy * self.pattern_bits;
            if accepted & (copy_mask << shift) != 0 {
                let copy_states = states.unpack(shift, copy_mask);
                self.emit_windows(ctx, owned, copy_states, position, copy_windows);
            }
        }
    }

    /// The bits of copy 0.
    fn copy_mask(&self) -> u64 {
        u64::MAX >> (64 - self.pattern_bits)
    }

    /// Repeats copy 0 of `bits` into `copies` copies.
    fn replicate(&self, bits: u64, copies: usize) -> u64 {
        let one_copy = bits & self.copy_mask();
        (0..copies).fold(0, |packed, copy| {
            packed | one_copy << (copy * self.pattern_bits)
        })
    }

    /// Returns single-copy states warmed up on the bytes before `start`.
    fn warmed_states(&self, haystack: &[u8], start: usize) -> EditStates {
        let mut states = EditStates::default();
        for &byte in &haystack[start.saturating_sub(self.longest_pattern + 1)..start] {
            states.advance(self.masks[usize::from(byte)], self.first_bits);
        }
        states
    }

    fn scan_serial(
        &self,
        ctx: &PrefilterContext<'_>,
        owned: &Range<usize>,
        mut states: EditStates,
        positions: Range<usize>,
        windows: &mut Vec<ApproximateCandidate>,
    ) {
        for position in positions {
            let byte = ctx.haystack[position];
            states.advance(self.masks[usize::from(byte)], self.first_bits);
            if states.accepted(self.accept_bits) != 0 {
                self.emit_windows(ctx, owned, states, position, windows);
            }
        }
    }

    /// Pushes each accepted pattern's best window ending at `position`, if it starts in
    /// `owned`. Out of line so the scan loop keeps its states in registers.
    #[inline(never)]
    fn emit_windows(
        &self,
        ctx: &PrefilterContext<'_>,
        owned: &Range<usize>,
        states: EditStates,
        position: usize,
        windows: &mut Vec<ApproximateCandidate>,
    ) {
        let mut accepted = states.accepted(self.accept_bits);
        while accepted != 0 {
            let bit = accepted.trailing_zeros() as usize;
            accepted &= accepted - 1;
            let pattern_index = usize::from(self.pattern_by_bit[bit]);
            let pattern_len = self.pattern_lens[pattern_index];
            let end = position + 1;
            // In preference order: lowest distance, then shortest.
            let best = [
                (states.exact, pattern_len, 0),
                (states.deleted, pattern_len - 1, 1),
                (states.substituted, pattern_len, 1),
                (states.inserted, pattern_len + 1, 1),
            ]
            .into_iter()
            .find(|&(state, len, _)| {
                state & (1 << bit) != 0
                    && len <= end
                    && (!ctx.line_local || stays_in_line(ctx.haystack, end - len, end))
            });
            if let Some((_, len, edit_distance)) = best
                && owned.contains(&(end - len))
            {
                windows.push(ApproximateCandidate::new(
                    end - len,
                    end,
                    pattern_index,
                    edit_distance,
                ));
            }
        }
    }
}

/// Whether `haystack[start..end]` stays within one `str::lines` line, excluding the CR of
/// a CRLF.
#[inline]
fn stays_in_line(haystack: &[u8], start: usize, end: usize) -> bool {
    memchr(b'\n', &haystack[start..end]).is_none()
        && !(haystack[end - 1] == b'\r' && haystack.get(end) == Some(&b'\n'))
}

/// Shift-And states per edit kind: bit `i` of a pattern's run is set when its bytes `..=i`
/// end at the last consumed byte unedited or with exactly one edit of that kind.
#[derive(Debug, Clone, Copy, Default)]
struct EditStates {
    exact: u64,
    substituted: u64,
    inserted: u64,
    deleted: u64,
}

impl EditStates {
    /// Consumes a byte whose pattern bits are `mask`. Bits a shift carries across patterns
    /// or copies land only where `first_bits` terms set them anyway, or are cleared.
    #[inline(always)]
    fn advance(&mut self, mask: u64, first_bits: u64) {
        let first = first_bits;
        let exact = ((self.exact << 1) | first) & mask;
        self.substituted = ((self.substituted << 1) & mask) | (self.exact << 1) | first;
        self.inserted = (((self.inserted << 1) | first) & mask) | self.exact;
        self.deleted = ((((self.deleted << 1) | (first << 1)) & mask) | (exact << 1)) & !first;
        self.exact = exact;
    }

    #[inline(always)]
    fn accepted(&self, accept_bits: u64) -> u64 {
        // `exact` is a subset of `substituted`.
        (self.substituted | self.inserted | self.deleted) & accept_bits
    }

    /// Extracts the copy at bit `shift` into copy 0.
    fn unpack(&self, shift: usize, copy_mask: u64) -> Self {
        Self {
            exact: (self.exact >> shift) & copy_mask,
            substituted: (self.substituted >> shift) & copy_mask,
            inserted: (self.inserted >> shift) & copy_mask,
            deleted: (self.deleted >> shift) & copy_mask,
        }
    }
}
