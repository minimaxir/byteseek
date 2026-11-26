use memchr::memchr;

/// Skip whitespace bytes.
#[inline(always)]
pub(super) fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b'\n' | b'\r') {
        pos += 1;
    }
    pos
}

/// Sets the high bit of each byte of `word` equal to `byte`. Only the lowest flag is exact,
/// since borrows can flag bytes above a match.
#[inline(always)]
fn swar_byte_matches(word: u64, byte: u8) -> u64 {
    const LOW_BITS: u64 = 0x0101_0101_0101_0101;
    let diff = word ^ (LOW_BITS * u64::from(byte));
    diff.wrapping_sub(LOW_BITS) & !diff & (LOW_BITS << 7)
}

/// Skip a JSON string starting at the opening `"`. Returns position after closing `"`.
#[inline]
pub(crate) fn skip_string(bytes: &[u8], pos: usize) -> usize {
    debug_assert!(pos < bytes.len() && bytes[pos] == b'"');
    // Fast path for short unescaped strings: SWAR-scan for the quote until a backslash shows up.
    const SWAR_WORDS: usize = 4;
    let mut word_start = pos + 1;
    for _ in 0..SWAR_WORDS {
        let Some(chunk) = bytes.get(word_start..).and_then(<[u8]>::first_chunk::<8>) else {
            break;
        };
        let word = u64::from_le_bytes(*chunk);
        let quotes = swar_byte_matches(word, b'"');
        let backslashes = swar_byte_matches(word, b'\\');
        if quotes != 0
            && (backslashes == 0 || quotes.trailing_zeros() < backslashes.trailing_zeros())
        {
            return word_start + quotes.trailing_zeros() as usize / 8 + 1;
        }
        if backslashes != 0 {
            break;
        }
        word_start += 8;
    }
    skip_escaped_string(bytes, pos)
}

/// Escape-aware string skipping: a quote preceded by an odd backslash run does not close it.
#[inline]
pub(super) fn skip_escaped_string(bytes: &[u8], pos: usize) -> usize {
    skip_string_rest(bytes, pos + 1)
}

/// Returns the position after the closing quote of the string containing `from`.
fn skip_string_rest(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while let Some(offset) = memchr(b'"', &bytes[i..]) {
        let quote_pos = i + offset;
        let backslashes = bytes[..quote_pos]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count();
        if backslashes % 2 == 0 {
            return quote_pos + 1;
        }
        i = quote_pos + 1;
    }
    bytes.len()
}

/// Skip a JSON value. `pos` points to the first byte of the value.
#[inline]
pub(super) fn skip_value(bytes: &[u8], pos: usize) -> usize {
    if pos >= bytes.len() {
        return pos;
    }
    match bytes[pos] {
        b'"' => skip_string(bytes, pos),
        b'{' => skip_container(bytes, pos, b'{', b'}'),
        b'[' => skip_container(bytes, pos, b'[', b']'),
        b't' | b'n' => literal_end(bytes, pos + 4),
        b'f' => literal_end(bytes, pos + 5),
        _ => {
            let mut i = pos;
            while i < bytes.len()
                && matches!(bytes[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                i += 1;
            }
            i
        }
    }
}

/// Returns the end of a `true`, `false` or `null` literal expected to end at `end`, clamped to
/// the input and moved to a char boundary so malformed input still yields a valid `str` span.
#[inline]
fn literal_end(bytes: &[u8], end: usize) -> usize {
    let mut end = end.min(bytes.len());
    // Skip UTF-8 continuation bytes (`10xxxxxx`).
    while bytes.get(end).is_some_and(|byte| byte & 0xC0 == 0x80) {
        end += 1;
    }
    end
}

/// Skips a JSON object or array, returning the position after the matching close, or
/// `bytes.len()` when it is unterminated. Only brackets of the container's own kind are counted.
pub(super) fn skip_container(bytes: &[u8], pos: usize, open: u8, close: u8) -> usize {
    container_end_within(bytes, pos, (open, close), bytes.len()).unwrap_or(bytes.len())
}

/// Like [`skip_container`], but returns `None` unless the container ends at or before `limit`.
///
/// Scans 64-byte blocks with bitmasks and falls back to a byte-wise scan for the tail and for
/// blocks with backslashes outside strings.
pub(super) fn container_end_within(
    bytes: &[u8],
    pos: usize,
    (open, close): (u8, u8),
    limit: usize,
) -> Option<usize> {
    debug_assert!(pos < bytes.len() && bytes[pos] == open);
    let mut scan = ContainerScan {
        open,
        close,
        depth: 1,
        in_string: false,
        escaped: false,
    };
    let mut block_start = pos + 1;

    while block_start < limit
        && let Some(block) = bytes
            .get(block_start..)
            .and_then(<[u8]>::first_chunk::<BLOCK_LEN>)
    {
        let closed_at = match scan.bitwise_block(block) {
            BlockOutcome::Closed(offset) => Some(offset),
            BlockOutcome::Open => None,
            // A string spanning a whole block is likely long: memchr to its end instead.
            BlockOutcome::InsideString => {
                block_start = skip_string_rest(bytes, block_start + BLOCK_LEN);
                scan.in_string = false;
                scan.escaped = false;
                continue;
            }
            BlockOutcome::NeedsScalar => scan.scalar(block),
        };
        if let Some(offset) = closed_at {
            return Some(block_start + offset + 1).filter(|&end| end <= limit);
        }
        block_start += BLOCK_LEN;
    }
    if block_start >= limit {
        return None;
    }
    scan.scalar(&bytes[block_start..])
        .map(|offset| block_start + offset + 1)
        .filter(|&end| end <= limit)
}

const BLOCK_LEN: usize = 64;

/// Bracket-matching state carried across blocks.
struct ContainerScan {
    open: u8,
    close: u8,
    depth: u32,
    in_string: bool,
    escaped: bool,
}

enum BlockOutcome {
    /// The container closes at this block offset.
    Closed(usize),
    /// The container is still open after the block.
    Open,
    /// The whole block lies inside one string that is still open after it.
    InsideString,
    /// A backslash outside a string makes escapes ambiguous; the state is untouched and the
    /// scalar scan must run.
    NeedsScalar,
}

/// Packs 64 zero-or-one flag bytes into a bitmask. One multiplication gathers each 8-byte
/// group into its top byte without carries, since the partial products never overlap.
#[inline(always)]
fn pack_flags(flags: &[u8; BLOCK_LEN]) -> u64 {
    flags
        .as_chunks::<8>()
        .0
        .iter()
        .enumerate()
        .fold(0, |mask, (group, chunk)| {
            let word = u64::from_le_bytes(*chunk);
            mask | ((word.wrapping_mul(0x0102_0408_1020_4080) >> 56) << (group * 8))
        })
}

/// Returns the bytes that follow an odd backslash run, carrying `escaped` across blocks.
///
/// This is simdjson's trick: adding the odd-position run starts to the backslash mask carries
/// through each run, flipping the parity of the byte after it.
#[inline(always)]
fn escaped_bytes(backslashes: u64, escaped: &mut bool) -> u64 {
    const EVEN_BITS: u64 = 0x5555_5555_5555_5555;
    let carried = u64::from(*escaped);
    // An escaped first byte is never itself an escape.
    let backslashes = backslashes & !carried;
    let follows_escape = (backslashes << 1) | carried;
    let odd_starts = backslashes & !EVEN_BITS & !follows_escape;
    let (even_start_runs, overflowed) = odd_starts.overflowing_add(backslashes);
    *escaped = overflowed;
    (EVEN_BITS ^ (even_start_runs << 1)) & follows_escape
}

/// Prefix XOR of `bits`, turning quote positions into an in-string mask.
#[inline(always)]
fn prefix_xor(mut bits: u64) -> u64 {
    bits ^= bits << 1;
    bits ^= bits << 2;
    bits ^= bits << 4;
    bits ^= bits << 8;
    bits ^= bits << 16;
    bits ^= bits << 32;
    bits
}

impl ContainerScan {
    /// Processes a block with bitmasks, assuming every backslash is inside a string. Blocks
    /// where that assumption fails return [`BlockOutcome::NeedsScalar`].
    #[inline(always)]
    fn bitwise_block(&mut self, block: &[u8; BLOCK_LEN]) -> BlockOutcome {
        // This loop auto-vectorizes. Opens and closes share one mask, and backslashes are
        // packed only when present.
        let mut quote_flags = [0_u8; BLOCK_LEN];
        let mut backslash_flags = [0_u8; BLOCK_LEN];
        let mut bracket_flags = [0_u8; BLOCK_LEN];
        for (index, &byte) in block.iter().enumerate() {
            quote_flags[index] = u8::from(byte == b'"');
            backslash_flags[index] = u8::from(byte == b'\\');
            bracket_flags[index] = u8::from(byte == self.open) | u8::from(byte == self.close);
        }
        let any_backslash = backslash_flags
            .as_chunks::<8>()
            .0
            .iter()
            .fold(0, |any, chunk| any | u64::from_ne_bytes(*chunk));
        let backslashes = if any_backslash == 0 {
            0
        } else {
            pack_flags(&backslash_flags)
        };
        let mut escaped = self.escaped;
        let mut quotes = pack_flags(&quote_flags);
        if backslashes != 0 || escaped {
            quotes &= !escaped_bytes(backslashes, &mut escaped);
        }
        let carried = if self.in_string { u64::MAX } else { 0 };
        let outside = !(prefix_xor(quotes) ^ carried);
        if backslashes & outside != 0 {
            return BlockOutcome::NeedsScalar;
        }
        self.escaped = escaped;
        if quotes == 0 && self.in_string {
            return BlockOutcome::InsideString;
        }
        self.in_string ^= quotes.count_ones() % 2 == 1;
        let mut brackets = pack_flags(&bracket_flags) & outside;
        while brackets != 0 {
            let offset = brackets.trailing_zeros() as usize;
            brackets &= brackets - 1;
            if block[offset] == self.open {
                self.depth += 1;
            } else {
                self.depth -= 1;
                if self.depth == 0 {
                    return BlockOutcome::Closed(offset);
                }
            }
        }
        BlockOutcome::Open
    }

    /// Byte-at-a-time version of [`Self::bitwise_block`], returning the closing offset if any.
    fn scalar(&mut self, bytes: &[u8]) -> Option<usize> {
        for (offset, &byte) in bytes.iter().enumerate() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
            } else if byte == b'"' {
                self.in_string = true;
            } else if byte == self.open {
                self.depth += 1;
            } else if byte == self.close {
                self.depth -= 1;
                if self.depth == 0 {
                    return Some(offset);
                }
            }
        }
        None
    }
}
