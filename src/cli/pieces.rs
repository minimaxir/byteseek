//! Reading input in pieces of whole lines, so a file's search holds a bounded part of it.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::mem;
use std::ops::Range;

use memchr::{memchr, memchr_iter, memrchr};

/// Most bytes a piece of an input searched alone reads, unless one line is longer: enough for
/// its lines to be split across threads.
pub const PIECE_LEN: usize = 2 * 1024 * 1024;
/// Most bytes a piece reads when pieces are searched side by side, one thread each, of many
/// files or of one: the memory they hold stays small however many threads there are, and a
/// piece stays short of the 512 KiB from which one search is split across threads.
pub const CONCURRENT_PIECE_LEN: usize = 256 * 1024;
/// Fewest bytes a piece reads, for inputs whose length is unknown or misreported as zero.
const MIN_PIECE_LEN: usize = 16 * 1024;
/// Bytes ripgrep reads first, before it searches any line: a NUL byte among them stops its
/// search before the first line.
const FIRST_READ_LEN: usize = 64 * 1024;
/// Bytes first read before a piece read at an offset, per line kept before it.
const KEPT_LINE_LEN: u64 = 256;
/// Bytes first read past a piece read at an offset, to find where its last line ends.
const LINE_END_LEN: usize = 4 * 1024;

/// What a NUL byte does to a search, as in ripgrep, which takes a file with one for binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    /// NUL bytes are searched like any other byte.
    Search,
    /// NUL bytes are searched as line feeds, and lines print up to the line of the first; a
    /// notice then says that the file matched, if any line did.
    Notice,
    /// The search stops at the line of the first NUL byte, with a warning if a line before it
    /// was selected and without the count or path it would have printed.
    Quit,
}

impl Binary {
    /// Whether the line of the first NUL byte ends what the search prints.
    pub fn stops(self) -> bool {
        matches!(self, Self::Notice | Self::Quit)
    }
}

/// Finds an input's first NUL byte as its bytes are read, if the search stops there, and turns
/// NUL bytes into line feeds for [`Binary::Notice`].
#[derive(Debug, Clone, Copy)]
pub struct Nuls {
    binary: Binary,
    first: Option<usize>,
}

impl Nuls {
    /// Looks at bytes for what `binary` does with NUL bytes.
    pub fn new(binary: Binary) -> Self {
        Self {
            binary,
            first: None,
        }
    }

    /// Looks at `bytes`, at `offset` in the input. Bytes read again, from before the first NUL
    /// byte found, are looked at again.
    pub fn scan(&mut self, bytes: &mut [u8], offset: usize) {
        let finds = self.binary.stops() && self.first.is_none_or(|first| offset < first);
        let converts = self.binary == Binary::Notice;
        if !finds && !converts {
            return;
        }
        let Some(mut at) = memchr(0, bytes) else {
            return;
        };
        if finds {
            self.first = Some(
                self.first
                    .map_or(offset + at, |first| first.min(offset + at)),
            );
        }
        if converts {
            loop {
                bytes[at] = b'\n';
                let Some(next) = memchr(0, &bytes[at + 1..]) else {
                    break;
                };
                at += next + 1;
            }
        }
    }

    /// The offset of the first NUL byte found.
    pub fn first(&self) -> Option<usize> {
        self.first
    }
}

/// Whole lines read from a source, one piece at a time. A piece can start with the last lines
/// of the one before it, searched again for context.
#[derive(Debug)]
pub struct Pieces<R> {
    reader: R,
    /// Bytes read and not yet passed, after the lines kept from the last piece.
    buffer: Vec<u8>,
    /// How many bytes `buffer` fills to before a piece is cut from it.
    target: usize,
    /// One past the last byte of the piece last returned.
    end: usize,
    /// Offset in the input of `buffer[0]`.
    offset: usize,
    eof: bool,
    nuls: Nuls,
    /// NUL bytes are left for the search of each piece to look for.
    deferred: bool,
    /// Lines kept at the front of the piece last returned, and where its new bytes start.
    kept: (usize, usize),
}

/// Lines read from the input, after any lines kept from the piece before.
#[derive(Debug)]
pub struct Piece<'a> {
    /// Whole lines; only the input's last line may lack a line feed.
    pub bytes: &'a [u8],
    /// Lines at the front of `bytes` that ended the piece before.
    pub kept_lines: usize,
    /// Where in `bytes` the lines not seen before start.
    pub new_start: usize,
    /// The input's first NUL byte, if the search stops there and it is in the piece's new
    /// bytes: the piece then ends at the start of its line, or at the input's start when
    /// ripgrep's first read holds it. ripgrep stops between those two, where a later read of
    /// its buffer started.
    pub nul: Option<usize>,
    /// The piece's lines from the NUL byte's line on.
    pub past_nul: &'a [u8],
    /// No one has looked for NUL bytes in the piece's new bytes: its search must, and ask
    /// for the piece again with [`Pieces::checked`] if it finds one.
    pub unchecked: bool,
    /// The line feeds of `bytes`, when they were counted as the piece was read.
    pub feeds: Option<usize>,
}

impl<'a> Piece<'a> {
    /// The piece of whole lines `bytes`, at `offset` in the input, that starts with
    /// `kept_lines` ending at `new_start`, cut at the line of `nul` if that is among its new
    /// bytes.
    pub fn new(
        bytes: &'a [u8],
        (kept_lines, new_start): (usize, usize),
        offset: usize,
        nul: Option<usize>,
    ) -> Self {
        let nul = nul.filter(|nul| (offset + new_start..offset + bytes.len()).contains(nul));
        // Kept lines end with a line feed, so the NUL byte's line starts among the new bytes.
        let end = nul.map_or(bytes.len(), |nul| {
            if nul < FIRST_READ_LEN {
                new_start
            } else {
                memrchr(b'\n', &bytes[..nul - offset]).map_or(0, |line_feed| line_feed + 1)
            }
        });
        let (bytes, past_nul) = bytes.split_at(end);
        Self {
            bytes,
            kept_lines,
            new_start,
            nul,
            past_nul,
            unchecked: false,
            feeds: None,
        }
    }

    /// The bytes not seen in an earlier piece.
    pub fn new_bytes(&self) -> &[u8] {
        &self.bytes[self.new_start..]
    }
}

impl<R: Read> Pieces<R> {
    /// Reads `reader`, whose length is `len` when known, in pieces of at most `piece_len`
    /// bytes unless a line is longer, doing with NUL bytes what `binary` says. A short input is
    /// read whole into a buffer of its size.
    pub fn new(reader: R, len: Option<u64>, piece_len: usize, binary: Binary) -> Self {
        // One byte more than the input lets the read that finds its end fit without growing.
        let target = len.map_or(piece_len, |len| {
            usize::try_from(len.saturating_add(1)).map_or(piece_len, |len| {
                len.clamp(MIN_PIECE_LEN.min(piece_len), piece_len)
            })
        });
        Self {
            reader,
            buffer: Vec::new(),
            target,
            end: 0,
            offset: 0,
            eof: false,
            nuls: Nuls::new(binary),
            deferred: false,
            kept: (0, 0),
        }
    }

    /// Leaves looking for NUL bytes to the search of each piece when `deferred`, for a search
    /// that finds them as it reads the bytes anyway. Pieces then come [`Piece::unchecked`]
    /// until one is asked for again with [`Self::checked`].
    #[must_use]
    pub fn deferring_nuls(mut self, deferred: bool) -> Self {
        self.deferred = deferred;
        self
    }

    /// Reads the next piece, which starts with the last `keep` lines of the piece before.
    ///
    /// # Returns
    ///
    /// The piece, or `None` once the input has no unread bytes.
    ///
    /// # Errors
    ///
    /// Returns the reader's error if a read fails.
    pub fn next(&mut self, keep: usize) -> io::Result<Option<Piece<'_>>> {
        self.kept = self.keep_lines(keep);
        let new_start = self.kept.1;
        loop {
            if !self.eof && self.buffer.len() < self.target {
                let (from, wanted) = (self.buffer.len(), self.target - self.buffer.len());
                let read = append(&mut self.reader, wanted as u64, &mut self.buffer)?;
                self.eof = read < wanted;
                if !self.deferred {
                    (self.nuls).scan(&mut self.buffer[from..], self.offset + from);
                }
            }
            if self.eof {
                if self.buffer.len() == new_start {
                    return Ok(None);
                }
                self.end = self.buffer.len();
                break;
            }
            if let Some(last_newline) = memrchr(b'\n', &self.buffer[new_start..]) {
                self.end = new_start + last_newline + 1;
                break;
            }
            if self.deferred {
                // NUL bytes searched as line feeds may end the line.
                self.check();
                continue;
            }
            // One line fills the buffer: grow until its end fits.
            self.target *= 2;
        }
        let mut piece = self.piece();
        piece.unchecked = self.deferred;
        Ok(Some(piece))
    }

    /// The piece last returned again, after looking for NUL bytes in all that was read: cut
    /// at the line of the first, or with its NUL bytes as line feeds, as the search needs.
    /// Later pieces come with their NUL bytes looked for.
    pub fn checked(&mut self) -> Piece<'_> {
        self.check();
        // NUL bytes that became line feeds may end lines past the piece's last.
        if !self.eof
            && let Some(last_newline) = memrchr(b'\n', &self.buffer[self.kept.1..])
        {
            self.end = self.kept.1 + last_newline + 1;
        }
        self.piece()
    }

    /// Looks for NUL bytes in the bytes read while that was deferred.
    fn check(&mut self) {
        if mem::take(&mut self.deferred) {
            self.nuls.scan(&mut self.buffer, self.offset);
        }
    }

    /// The piece that ends at `end`.
    fn piece(&self) -> Piece<'_> {
        Piece::new(
            &self.buffer[..self.end],
            self.kept,
            self.offset,
            self.nuls.first(),
        )
    }

    /// Moves the last `keep` lines of the piece last returned, and the bytes read after it, to
    /// the front of the buffer. Returns how many lines were kept and where the bytes after them
    /// start.
    fn keep_lines(&mut self, keep: usize) -> (usize, usize) {
        let piece = &self.buffer[..self.end];
        // Each line of a piece that has a next one ends with a line feed, so the last `keep`
        // lines start just past the line feed `keep + 1` from the end.
        let kept_from = memchr_iter(b'\n', piece)
            .rev()
            .nth(keep)
            .map_or(0, |line_feed| line_feed + 1);
        let kept_lines = memchr_iter(b'\n', &piece[kept_from..]).count();
        let new_start = self.end - kept_from;
        self.buffer.drain(..kept_from);
        self.offset += kept_from;
        self.end = 0;
        (kept_lines, new_start)
    }
}

/// Reads the lines of `file` that start in the byte range `segment`, after the last `keep`
/// lines before them, into `buffer`, doing with NUL bytes what `binary` says. The last line
/// runs on past the segment to its end, so the segments of a file give each of its lines to
/// one piece.
///
/// # Returns
///
/// The piece, with its offset in the file, or `None` when no line starts in the segment.
///
/// # Errors
///
/// Returns the error of a failed seek or read.
pub fn read_piece_at<'b>(
    file: &mut File,
    segment: Range<u64>,
    keep: usize,
    binary: Binary,
    buffer: &'b mut Vec<u8>,
) -> io::Result<Option<Piece<'b>>> {
    let mut nuls = Nuls::new(binary);
    // The bytes before the segment show where its first line starts and hold the lines kept;
    // they are read again from further back when the kept lines are longer. Reading a little
    // past the segment usually finds where its last line ends in the same read.
    let mut back = (keep as u64 + 1) * KEPT_LINE_LEN;
    let (base, filled, start, kept_from, kept_lines) = loop {
        let base = segment.start.saturating_sub(back);
        file.seek(SeekFrom::Start(base))?;
        let wanted = (segment.end - base) as usize + LINE_END_LEN;
        let filled = fill(file, buffer, 0..wanted)?;
        nuls.scan(&mut buffer[..filled], base as usize);
        let (head, tail) = ((segment.start - base) as usize, wanted - LINE_END_LEN);
        let start = if head == 0 {
            0
        } else {
            // A line starts in the segment after a line feed from the byte before it to the
            // byte before its last.
            let searched = buffer.get(head - 1..filled.min(tail).saturating_sub(1));
            match searched.and_then(|searched| memchr(b'\n', searched)) {
                Some(at) => head + at,
                None => return Ok(None),
            }
        };
        if start >= filled {
            return Ok(None);
        }
        let mut line_feeds = memchr_iter(b'\n', &buffer[..start]).rev();
        match line_feeds.nth(keep) {
            Some(line_feed) => break (base, filled, start, line_feed + 1, keep),
            None if base == 0 => {
                let kept_lines = memchr_iter(b'\n', &buffer[..start]).count();
                break (base, filled, start, 0, kept_lines);
            }
            None => back *= 4,
        }
    };
    // The last line is the one holding the segment's last byte.
    let last_byte = (segment.end - base) as usize - 1;
    let mut end = match buffer[..filled]
        .get(last_byte..)
        .and_then(|rest| memchr(b'\n', rest))
    {
        Some(at) => last_byte + at + 1,
        None => filled,
    };
    // The read filled up without ending that line, so it runs on past what was read.
    if end == filled && filled > last_byte + LINE_END_LEN {
        let mut step = LINE_END_LEN;
        loop {
            let from = end;
            end = fill(file, buffer, from..from + step)?;
            nuls.scan(&mut buffer[from..end], base as usize + from);
            if let Some(at) = memchr(b'\n', &buffer[from..end]) {
                end = from + at + 1;
                break;
            }
            if end < from + step {
                break;
            }
            step *= 2;
        }
    }
    Ok(Some(Piece::new(
        &buffer[kept_from..end],
        (kept_lines, start - kept_from),
        base as usize + kept_from,
        nuls.first(),
    )))
}

/// Reads `file` from where it is into `buffer[range]` until the range fills or the file ends,
/// and returns where the bytes read end. The buffer keeps its length between reads, so it is
/// only zeroed as it grows.
fn fill(file: &mut File, buffer: &mut Vec<u8>, range: Range<usize>) -> io::Result<usize> {
    if let Some(more) = range.end.checked_sub(buffer.len()) {
        // Exact growth keeps a buffer that a piece's last line outgrows from doubling.
        buffer.reserve_exact(more);
        buffer.resize(range.end, 0);
    }
    let mut filled = range.start;
    while filled < range.end {
        match file.read(&mut buffer[filled..range.end]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Reads up to `len` more bytes of `reader` onto the end of `buffer`. Reading into spare
/// capacity leaves it unzeroed.
fn append(reader: impl Read, len: u64, buffer: &mut Vec<u8>) -> io::Result<usize> {
    buffer.reserve_exact(usize::try_from(len).unwrap_or(0));
    reader.take(len).read_to_end(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns at most `step` bytes per read, like a pipe.
    struct ShortReads<'a> {
        remaining: &'a [u8],
        step: usize,
    }

    impl Read for ShortReads<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let len = self.step.min(buffer.len()).min(self.remaining.len());
            buffer[..len].copy_from_slice(&self.remaining[..len]);
            self.remaining = &self.remaining[len..];
            Ok(len)
        }
    }

    /// A piece's new bytes and how many lines it kept.
    type PieceRead = (Vec<u8>, usize);

    /// Every piece of `input`, checking each against the input at its offset.
    fn read_all(input: &[u8], step: usize, keep: usize) -> Vec<PieceRead> {
        let reader = ShortReads {
            remaining: input,
            step,
        };
        let mut pieces = Pieces::new(reader, None, PIECE_LEN, Binary::Search);
        let (mut read, mut new_from) = (Vec::new(), 0);
        while let Some(piece) = pieces.next(keep).expect("reads from memory succeed") {
            let kept = &piece.bytes[..piece.new_start];
            assert_eq!(memchr_iter(b'\n', kept).count(), piece.kept_lines);
            let offset = new_from - piece.new_start;
            assert_eq!(&input[offset..offset + piece.bytes.len()], piece.bytes);
            new_from += piece.new_bytes().len();
            read.push((piece.new_bytes().to_vec(), piece.kept_lines));
        }
        read
    }

    fn concat_new(read: &[PieceRead]) -> Vec<u8> {
        read.iter().flat_map(|(bytes, _)| bytes.clone()).collect()
    }

    #[test]
    fn pieces_cover_the_input_once_in_whole_lines() {
        let line = b"0123456789 abcdefghij\n";
        let mut input = line.repeat(3 * PIECE_LEN / line.len());
        input.extend_from_slice(b"last line without a line feed");
        for step in [7, 4096, usize::MAX] {
            let read = read_all(&input, step, 0);

            assert!(read.len() > 2);
            assert_eq!(concat_new(&read), input);
            for (bytes, kept) in &read[..read.len() - 1] {
                assert_eq!(bytes.last(), Some(&b'\n'));
                assert_eq!(*kept, 0);
            }
        }
    }

    #[test]
    fn pieces_start_with_the_lines_kept_from_the_last() {
        let input: Vec<u8> = (0..300_000)
            .flat_map(|index| format!("line {index}\n").into_bytes())
            .collect();

        let read = read_all(&input, usize::MAX, 2);

        assert!(read.len() > 1);
        assert_eq!(concat_new(&read), input);
        assert_eq!(read[0].1, 0);
        assert!(read[1..].iter().all(|(_, kept)| *kept == 2));
    }

    #[test]
    fn a_line_longer_than_a_piece_grows_the_buffer() {
        let mut input = vec![b'x'; 2 * PIECE_LEN + 5];
        input.extend_from_slice(b"\nshort\n");

        let read = read_all(&input, usize::MAX, 1);

        assert_eq!(concat_new(&read), input);
        assert!(read[0].0.len() >= 2 * PIECE_LEN + 6);
    }

    /// Every piece of `input` read at offsets, `segment_len` bytes apart, checking each.
    fn read_all_at(input: &[u8], segment_len: u64, keep: usize) -> Vec<PieceRead> {
        let mut temporary = tempfile::NamedTempFile::new().expect("create a temporary file");
        io::Write::write_all(&mut temporary, input).expect("write the temporary file");
        let mut file = File::open(temporary.path()).expect("the temporary file opens");
        let (len, mut buffer, mut read) = (input.len() as u64, Vec::new(), Vec::new());
        let mut new_from = 0;
        for start in (0..len).step_by(segment_len as usize) {
            let segment = start..len.min(start + segment_len);
            let piece = read_piece_at(&mut file, segment, keep, Binary::Search, &mut buffer);
            let Some(piece) = piece.expect("the file reads") else {
                continue;
            };
            let lines_before = memchr_iter(b'\n', &input[..new_from]).count();
            let offset = new_from - piece.new_start;
            assert_eq!(&input[offset..offset + piece.bytes.len()], piece.bytes);
            new_from += piece.new_bytes().len();
            assert_eq!(piece.kept_lines, keep.min(lines_before));
            assert_eq!(
                memchr_iter(b'\n', &piece.bytes[..piece.new_start]).count(),
                piece.kept_lines
            );
            read.push((piece.new_bytes().to_vec(), piece.kept_lines));
        }
        read
    }

    #[test]
    fn pieces_read_at_offsets_cover_the_input_once_in_whole_lines() {
        let mut input = Vec::new();
        for index in 0..400 {
            input.extend_from_slice(&b"x".repeat(index % 37));
            input.push(b'\n');
        }
        input.extend_from_slice(&b"y".repeat(3 * LINE_END_LEN + 11));
        input.extend_from_slice(b"\n\nlast line without a line feed");
        for segment_len in [1, 5, 64, 4096] {
            for keep in [0, 2, 40] {
                let read = read_all_at(&input, segment_len, keep);

                assert_eq!(concat_new(&read), input, "{segment_len} {keep}");
                for (bytes, _) in &read[..read.len() - 1] {
                    assert_eq!(bytes.last(), Some(&b'\n'));
                }
            }
        }
    }

    #[test]
    fn a_line_longer_than_many_segments_is_read_by_the_one_it_starts_in() {
        // The first line spans the first segment, so the long line starts in the second.
        let mut input = b"s".repeat(150);
        input.push(b'\n');
        input.extend_from_slice(&b"z".repeat(5 * LINE_END_LEN));
        input.extend_from_slice(b"\nend\n");

        let read = read_all_at(&input, 100, 1);

        assert_eq!(concat_new(&read), input);
        assert_eq!(read.len(), 3);
        assert_eq!(read[1].0.len(), 5 * LINE_END_LEN + 1);
    }

    #[test]
    fn nul_bytes_are_found_or_converted_as_the_search_needs() {
        let mut bytes = *b"a\0b\0c";
        let mut quit = Nuls::new(Binary::Quit);
        quit.scan(&mut bytes, 10);
        quit.scan(&mut bytes.clone(), 20);
        assert_eq!((quit.first(), &bytes), (Some(11), b"a\0b\0c"));

        let mut notice = Nuls::new(Binary::Notice);
        let mut later = *b"x\0";
        notice.scan(&mut later, 7);
        notice.scan(&mut [0], 3);
        assert_eq!((notice.first(), &later), (Some(3), b"x\n"));

        let mut search = Nuls::new(Binary::Search);
        search.scan(&mut bytes, 0);
        assert_eq!((search.first(), &bytes), (None, b"a\0b\0c"));
    }

    #[test]
    fn a_piece_holding_the_first_nul_ends_at_its_line() {
        let mut input = b"one\n".repeat(PIECE_LEN / 2);
        let nul = input.len() - 2;
        input[nul] = 0;
        input.extend_from_slice(b"late\0\n");
        for binary in [Binary::Quit, Binary::Notice] {
            let mut pieces = Pieces::new(&input[..], None, PIECE_LEN, binary);
            let (mut cut, mut new_from) = (None, 0);
            while let Some(piece) = pieces.next(1).expect("reads from memory succeed") {
                new_from += piece.new_bytes().len();
                if let Some(at) = piece.nul {
                    cut = Some((at, new_from, piece.past_nul.to_vec()));
                }
                new_from += piece.past_nul.len();
            }
            let expected: &[u8] = match binary {
                Binary::Notice => b"on\n\nlate\n\n",
                _ => b"on\0\nlate\0\n",
            };
            let (at, end, past_nul) = cut.expect("a piece holds the NUL byte");
            assert_eq!(
                (at, end, &past_nul[..]),
                (nul, nul - 2, expected),
                "{binary:?}"
            );
        }

        let mut file = tempfile::NamedTempFile::new().expect("create a temporary file");
        io::Write::write_all(&mut file, &input).expect("write the temporary file");
        let mut reader = File::open(file.path()).expect("the temporary file opens");
        let mut buffer = Vec::new();
        let len = input.len() as u64;
        let piece = read_piece_at(&mut reader, 8..len, 2, Binary::Quit, &mut buffer)
            .expect("the file reads")
            .expect("lines start in the segment");
        assert_eq!(piece.nul, Some(nul));
        assert_eq!(piece.new_bytes().len(), nul - 2 - 8);
        assert_eq!(piece.past_nul, b"on\0\nlate\0\n");
    }

    /// A piece's new bytes, its NUL byte and its lines from there on.
    type CutPiece = (Vec<u8>, Option<usize>, Vec<u8>);

    /// Every piece of `input`. If `deferred`, NUL bytes are left to a search of each piece,
    /// which asks for the piece again when it holds one.
    fn read_cut(input: &[u8], binary: Binary, deferred: bool) -> Vec<CutPiece> {
        let mut pieces = Pieces::new(input, None, PIECE_LEN, binary).deferring_nuls(deferred);
        let (mut read, mut unchecked) = (Vec::new(), deferred);
        while let Some(mut piece) = pieces.next(1).expect("reads from memory succeed") {
            // Once one piece is checked, so is every later one.
            assert!(unchecked || !piece.unchecked);
            if piece.unchecked && memchr(0, piece.bytes).is_some() {
                piece = pieces.checked();
            }
            unchecked = piece.unchecked;
            read.push((
                piece.new_bytes().to_vec(),
                piece.nul,
                piece.past_nul.to_vec(),
            ));
        }
        read
    }

    #[test]
    fn pieces_checked_for_nul_bytes_by_their_search_are_the_pieces_checked_as_read() {
        let mut late = b"one\n".repeat(PIECE_LEN);
        let nul = 3 * PIECE_LEN + 1;
        late[nul] = 0;
        late[nul + 9] = 0;
        let mut early = late.clone();
        early[2] = 0;
        // No line feed ends a line before the buffer fills, unless a NUL byte becomes one.
        let mut unbroken = b"ab\0".repeat(PIECE_LEN);
        unbroken.extend_from_slice(b"\nend\n");
        let clean = b"one\n".repeat(PIECE_LEN);

        for binary in [Binary::Quit, Binary::Notice] {
            for input in [&late, &early, &unbroken, &clean] {
                let deferred = read_cut(input, binary, true);

                assert_eq!(deferred, read_cut(input, binary, false), "{binary:?}");
            }
        }
        let cut = read_cut(&late, Binary::Quit, true);
        assert_eq!(cut.iter().find_map(|piece| piece.1), Some(nul));
        assert!(cut[0].1.is_none() && cut.len() > 1);
    }

    #[test]
    fn a_short_input_is_one_piece_and_an_empty_one_none() {
        let mut pieces = Pieces::new(&b"a\nb"[..], Some(3), PIECE_LEN, Binary::Search);
        let piece = pieces.next(5).expect("reads succeed").expect("one piece");
        assert_eq!((piece.bytes, piece.kept_lines), (&b"a\nb"[..], 0));
        assert!(pieces.next(5).expect("reads succeed").is_none());

        assert!(read_all(b"", 1, 0).is_empty());
        assert!(read_all(b"\n", 1, 3).len() == 1);
    }
}
