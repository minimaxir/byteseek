use std::io::{self, ErrorKind, Read};

use memchr::memrchr;

use crate::exact::Extent;

use super::lines::PlannedLineCounter;
use super::{Engine, Searcher};

/// Bytes read per step, small enough to stay in cache between the read and the search.
const STREAM_CHUNK_LEN: usize = 256 * 1024;

impl Searcher {
    /// Counts lines that contain at least one match in text read from `reader`.
    ///
    /// For UTF-8 text the count equals [`Searcher::count_matching_lines`] on the whole text.
    /// Non-structured searchers also accept text that is not UTF-8: lines split at line feeds
    /// like `str::lines`, and patterns match byte for byte, so `café` matches UTF-8 but not
    /// Latin-1. They stream through one reused buffer, holding only the unfinished last line.
    /// Text already in memory is counted faster by [`Searcher::count_matching_lines`], which
    /// can use more threads. Structured searchers read the whole text into memory first.
    ///
    /// # Arguments
    ///
    /// * `reader` - Source of text, such as an open file.
    ///
    /// # Returns
    ///
    /// Number of lines with at least one accepted match.
    ///
    /// # Errors
    ///
    /// Returns the reader's error if a read fails. A structured searcher returns
    /// [`ErrorKind::InvalidData`] if the text is not UTF-8; text that does not parse counts
    /// zero lines.
    ///
    /// # Examples
    ///
    /// ```
    /// use byteseek::{SearchConfig, Searcher};
    ///
    /// let searcher = Searcher::new(["ERROR"], SearchConfig::default())?;
    /// let log = "INFO ready\nERROR failed\nERROR again\n";
    /// // `&[u8]` implements `Read`, standing in for a file here.
    /// let count = searcher.count_matching_lines_in_reader(log.as_bytes())?;
    /// assert_eq!(count, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn count_matching_lines_in_reader(&self, mut reader: impl Read) -> io::Result<usize> {
        if matches!(self.engine, Engine::Structured(_)) {
            let mut text = String::new();
            reader.read_to_string(&mut text)?;
            return Ok(self.count_matching_lines(&text));
        }

        let mut buffer = vec![0_u8; STREAM_CHUNK_LEN];
        // Bytes at the front of `buffer` from a line whose end is not read yet.
        let mut pending = 0_usize;
        let mut count = 0_usize;
        let mut counted = 0_usize;
        let mut counter = None;
        loop {
            if pending == buffer.len() {
                // One line fills the buffer: grow until its end fits.
                buffer.resize(2 * buffer.len(), 0);
            }
            let read = match reader.read(&mut buffer[pending..]) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            let filled = pending + read;
            let Some(last_newline) = memrchr(b'\n', &buffer[pending..filled]) else {
                pending = filled;
                continue;
            };
            let complete = pending + last_newline + 1;
            count += self.count_line_aligned(&buffer[..complete], counted, &mut counter);
            counted += complete;
            buffer.copy_within(complete..filled, 0);
            pending = filled - complete;
        }
        Ok(count + self.count_line_aligned(&buffer[..pending], counted, &mut counter))
    }

    /// Counts the matching lines of `bytes`, whole lines in memory that need not be UTF-8, on
    /// this thread and as [`Searcher::count_matching_lines_in_reader`] counts them. Needs a
    /// searcher that is not structured.
    #[cfg(feature = "cli")]
    pub(crate) fn count_matching_lines_in_bytes(&self, bytes: &[u8]) -> usize {
        self.count_line_aligned(bytes, 0, &mut None)
    }

    /// Counts the matching lines of `piece`, whole lines that follow `preceded_by` bytes of
    /// input. The first piece creates the shared `counter`.
    fn count_line_aligned<'s>(
        &'s self,
        piece: &[u8],
        preceded_by: usize,
        counter: &mut Option<PlannedLineCounter<'s>>,
    ) -> usize {
        if self.searches_lines_one_by_one() {
            return self.count_lines_one_by_one(piece);
        }
        let extent = Extent::Piece { preceded_by };
        let counter = counter.get_or_insert_with(|| PlannedLineCounter::new(self, piece, extent));
        counter.weigh_scans(piece, extent);
        counter.count(piece)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::STREAM_CHUNK_LEN;
    use crate::{CaseMode, SearchConfig, Searcher, StructuredMode};

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

    fn corpus() -> String {
        let long_line = "x".repeat(2 * STREAM_CHUNK_LEN + 17);
        let mut text = String::new();
        for index in 0..9_000 {
            let level = ["INFO", "warn", "ERROR", "Error"][index % 4];
            text.push_str(&format!("{index} {level} status={}\r\n", 200 + index % 7));
            if index == 4_000 {
                // Longer than the buffer, with a match at its very end.
                text.push_str(&long_line);
                text.push_str(" ERROR\n\n");
            }
        }
        text.push_str("last line ERROR without a newline");
        text
    }

    #[test]
    fn reader_counts_equal_whole_text_counts_for_every_engine() {
        let text = corpus();
        let insensitive = SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .build();
        let one_edit = SearchConfig::builder().max_edit_distance(Some(1));
        let searchers = [
            Searcher::new(["ERROR"], SearchConfig::default()),
            Searcher::new(["error"], insensitive),
            Searcher::new(["error", "warn", "status=203"], insensitive),
            Searcher::new(["ERROR", "status=201\r"], SearchConfig::default()),
            Searcher::new(
                ["ERROR"],
                SearchConfig::builder().max_matches(Some(1)).build(),
            ),
            Searcher::new(
                ["(ERROR OR warn) AND NOT status=204"],
                SearchConfig::builder().boolean_queries(true).build(),
            ),
            Searcher::new(["EROR"], one_edit.build()),
            Searcher::new(["EROR", "sttus=203", "wurn"], one_edit.build()),
            Searcher::new(
                ["eror", "WURN"],
                one_edit.case_mode(CaseMode::AsciiInsensitive).build(),
            ),
            // A pattern too short to split into halves, and a whole-word search.
            Searcher::new(["ERO"], one_edit.build()),
            Searcher::new(["ERRORS"], one_edit.whole_word(true).build()),
        ];
        for searcher in searchers {
            let searcher = searcher.expect("test searchers compile");
            let expected = searcher.count_matching_lines(&text);
            assert!(expected > 0, "{searcher:?}");
            for step in [1_000, STREAM_CHUNK_LEN, usize::MAX] {
                let reader = ShortReads {
                    remaining: text.as_bytes(),
                    step,
                };

                let count = searcher
                    .count_matching_lines_in_reader(reader)
                    .expect("a reader of bytes in memory does not fail");

                assert_eq!(count, expected, "{searcher:?} with reads of {step}");
            }
        }
    }

    #[test]
    fn a_long_stream_counts_like_the_whole_text_whatever_plan_its_pieces_share() {
        // Long enough for a pair scan to be weighed mid-stream, with pattern sets for which
        // that matters.
        let text = (0..40_000)
            .map(|index| {
                let level = ["INFO", "warn", "ERROR", "Error", "TRACE", "debug"][index % 6];
                format!(
                    "{index:05} {level} request_id={index} status={}\n",
                    200 + index % 9
                )
            })
            .collect::<String>();
        assert!(text.len() > 4 * STREAM_CHUNK_LEN);
        let sets: [&[&str]; 2] = [
            &[
                "ERROR",
                "warn",
                "TRACE",
                "status=204",
                "request_id=1999",
                "id=77",
            ],
            &["status=204", "status=207", "request_id=1999"],
        ];
        for patterns in sets {
            let searcher =
                Searcher::new(patterns, SearchConfig::default()).expect("literal patterns compile");
            let expected = searcher.count_matching_lines(&text);
            assert!(expected > 0, "{patterns:?}");
            for step in [1_000, usize::MAX] {
                let reader = ShortReads {
                    remaining: text.as_bytes(),
                    step,
                };

                let count = searcher
                    .count_matching_lines_in_reader(reader)
                    .expect("a reader of bytes in memory does not fail");

                assert_eq!(count, expected, "{patterns:?} with reads of {step}");
            }
        }
    }

    #[test]
    fn reader_counts_lines_that_are_not_utf8_whatever_the_read_size() {
        // Five lines, of which only the fourth is UTF-8.
        let input = &b"a b\xff\ncaf\xe9 a\r\n\xff\xfe\nb then a\n\x80abcd"[..];
        let builder = SearchConfig::builder;
        let one_edit = builder().max_edit_distance(Some(1)).build();
        let searchers = [
            (Searcher::new(["a"], builder().build()), 4),
            (Searcher::new(["a", "b"], builder().build()), 4),
            (
                Searcher::new(
                    ["A"],
                    builder().case_mode(CaseMode::AsciiInsensitive).build(),
                ),
                4,
            ),
            (
                Searcher::new(["a AND b"], builder().boolean_queries(true).build()),
                3,
            ),
            (Searcher::new(["abcd"], one_edit), 1),
            // This and the rest are searched line by line.
            (
                Searcher::new(["a"], builder().max_matches(Some(1)).build()),
                4,
            ),
            (Searcher::new(["a\nb"], builder().build()), 0),
            (Searcher::new(["abc"], one_edit), 1),
        ];
        for (searcher, expected) in searchers {
            let searcher = searcher.expect("test searchers compile");
            for step in [1, 3, 7, usize::MAX] {
                let reader = ShortReads {
                    remaining: input,
                    step,
                };

                let count = searcher.count_matching_lines_in_reader(reader);

                assert_eq!(
                    count.ok(),
                    Some(expected),
                    "{searcher:?} with reads of {step}"
                );
            }
        }
    }

    #[test]
    fn reader_counts_no_lines_in_empty_input() {
        let one_edit = SearchConfig::builder().max_edit_distance(Some(1)).build();
        let searchers = [
            Searcher::new(["a", "b"], SearchConfig::default()),
            Searcher::new(["a\nb"], SearchConfig::default()),
            Searcher::new(["abcd"], one_edit),
        ];
        for searcher in searchers {
            let searcher = searcher.expect("test searchers compile");

            let count = searcher.count_matching_lines_in_reader(io::empty());

            assert_eq!(count.ok(), Some(0), "{searcher:?}");
        }
    }

    #[test]
    fn a_long_stream_that_is_not_utf8_counts_its_matching_lines() {
        // Long enough to weigh a pair scan; every line holds invalid UTF-8.
        let line_count = 140_000_usize;
        let (matching, other) = (
            &b"ERROR caf\xe9 failed\n"[..],
            &b"INFO \xff\xfe ready\n"[..],
        );
        let mut input = Vec::with_capacity(line_count * matching.len());
        for index in 0..line_count {
            input.extend_from_slice(if index % 7 == 0 { matching } else { other });
        }
        assert!(input.len() > 4 * STREAM_CHUNK_LEN);
        let expected = line_count.div_ceil(7);
        let builder = SearchConfig::builder;
        let searchers = [
            Searcher::new(["ERROR"], builder().build()),
            Searcher::new(["ERROR", "failed"], builder().build()),
            Searcher::new(
                ["ERROR", "failed", "WARN", "TRACE", "status=500", "fatal"],
                builder().build(),
            ),
            Searcher::new(
                ["error", "FAILED"],
                builder().case_mode(CaseMode::AsciiInsensitive).build(),
            ),
            Searcher::new(["ERROR"], builder().whole_word(true).build()),
            Searcher::new(
                ["ERROR AND failed"],
                builder().boolean_queries(true).build(),
            ),
            Searcher::new(["EROR"], builder().max_edit_distance(Some(1)).build()),
        ];
        for searcher in searchers {
            let searcher = searcher.expect("test searchers compile");

            let count = searcher.count_matching_lines_in_reader(input.as_slice());

            assert_eq!(count.ok(), Some(expected), "{searcher:?}");
        }
    }

    #[test]
    fn a_structured_reader_rejects_text_that_is_not_utf8() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build();
        let searcher = Searcher::new(["$.name"], config).expect("a path pattern compiles");

        let count = searcher.count_matching_lines_in_reader(&b"{\"name\": \"\xff\"}"[..]);

        assert_eq!(
            count.map_err(|error| error.kind()),
            Err(io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn multibyte_text_and_newline_patterns_survive_any_read_size() {
        // Every read size cuts some two- to four-byte character and some CRLF pair.
        let text = "caf\u{e9} \u{20ac}5 \u{1f600} done\r\nplain line\n".repeat(700);
        let searchers = [
            Searcher::new(["\u{20ac}5"], SearchConfig::default()),
            Searcher::new(["\u{e9}", "plain"], SearchConfig::default()),
            // Spans two lines, so no line matches.
            Searcher::new(["done\r\nplain"], SearchConfig::default()),
        ];
        for (searcher, matches_lines) in searchers.into_iter().zip([true, true, false]) {
            let searcher = searcher.expect("test searchers compile");
            let expected = searcher.count_matching_lines(&text);
            assert_eq!(expected > 0, matches_lines);
            for step in [1, 3, 7, 4_096] {
                let reader = ShortReads {
                    remaining: text.as_bytes(),
                    step,
                };

                let count = searcher
                    .count_matching_lines_in_reader(reader)
                    .expect("a reader of bytes in memory does not fail");

                assert_eq!(count, expected, "{searcher:?} with reads of {step}");
            }
        }
    }
}
