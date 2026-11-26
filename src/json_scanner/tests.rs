use memchr::memmem;

use super::batch::JsonPathBatch;
use super::bytes::{container_end_within, skip_container, skip_escaped_string};
use super::parallel::{ElementShape, PARALLEL_SCAN_LEN, PrefilteredSplitScan, SplitScan};
use super::{
    JsonStream, PathSegment, json_fold_small_document, json_fold_values_with_paths,
    json_line_value_start, json_navigate_path, json_scan_values, json_scan_values_prefiltered,
    json_scan_values_with_paths, json_short_stream_value, json_stream_extent, json_stream_value,
    parse_path_pattern,
};
use crate::occurrences::{Needle, OccurrenceCursor, SEARCH_WINDOW};

fn record_list(count: usize) -> String {
    let records = (0..count)
        .map(|index| {
            format!(
                concat!(
                    r#"{{"id":{index},"tags":["a","critical"],"#,
                    r#""user":{{"name":"u{index}","ok":true}},"q":"x\"critical\\"}}"#,
                ),
                index = index,
            )
        })
        .collect::<Vec<_>>();
    format!("[{}]", records.join(", "))
}

/// Records whose nested objects share the elements' first key (and second, if requested).
fn package_list(count: usize, shared_second_key: bool) -> String {
    let second_key = if shared_second_key { "version" } else { "req" };
    let records = (0..count)
        .map(|index| {
            format!(
                concat!(
                    r#"{{"name":"p{index}","version":"1.{index}","deps":[{{"name":"a","#,
                    r#""{second_key}":"1"}},{{"name":"critical","{second_key}":"2"}}]}}"#,
                ),
                index = index,
                second_key = second_key,
            )
        })
        .collect::<Vec<_>>();
    format!("[{}]", records.join(","))
}

#[test]
fn element_shapes_reject_nested_objects_with_other_second_keys() {
    let count = 200;
    let packages = package_list(count, false);
    let bytes = packages.as_bytes();
    let shape = ElementShape::of_object(bytes, 1).expect("records are objects with keys");

    let starts = shape
        .starts_between(bytes, 0, bytes.len())
        .collect::<Vec<_>>();
    let record_starts = memmem::find_iter(bytes, br#",{"name":"p"#)
        .map(|comma| comma + 1)
        .collect::<Vec<_>>();

    assert_eq!(starts.len(), count - 1);
    assert_eq!(starts, record_starts);
    assert!(ElementShape::of_object(br#"{"only":[1,{"a":2}]}"#, 0).is_some());
    assert!(ElementShape::of_object(br#"{"a":1 "b":2}"#, 0).is_none());
    assert!(ElementShape::of_object(b"{}", 0).is_none());
}

#[test]
fn root_skip_checks_match_walking_the_whole_container() {
    let cases: [(&[u8], &[u8]); 7] = [
        (br#"{"a":"x"}"#, b"zz"),
        (br#"{"a":"zz"}"#, b"zz"),
        (br#"{"a":"x"} "zz""#, b"zz"),
        (br#"{"a":"x"} "#, b"x\"}"),
        (br#"{"a":"x"} "#, b"\"} "),
        (br#"{"a":"x""#, b"x"),
        (br#"{"a":"x"}"#, b""),
    ];

    for (bytes, needle) in cases {
        let finder = Needle::new(needle, false);
        let scan = PrefilteredSplitScan {
            bytes,
            needle: &finder,
            min_depth: 0,
            matcher: &|_: &[u8]| true,
            paths: std::marker::PhantomData::<fn() -> String>,
        };
        let end = skip_container(bytes, 0, b'{', b'}');

        assert_eq!(
            scan.skips_container(0, None, 0),
            memmem::find(&bytes[..end], needle).is_none(),
            "{:?} with needle {:?}",
            String::from_utf8_lossy(bytes),
            String::from_utf8_lossy(needle)
        );
    }
}

#[test]
fn parallel_array_scans_match_sequential_scans() {
    let list = record_list(PARALLEL_SCAN_LEN / 40);
    let documents = [
        list.clone(),
        format!(r#"{{"before":1,"events":{list},"nested":{{"deep":{list}}},"after":"critical"}}"#),
        // Nested objects shaped like the elements produce false element starts.
        package_list(PARALLEL_SCAN_LEN / 80, true),
        package_list(PARALLEL_SCAN_LEN / 80, false),
        // Malformed: falls back to the sequential scan.
        list.replacen(r#"{"id":7,"#, r#"{"id":7 "#, 1),
    ];

    for (document_index, document) in documents.iter().enumerate() {
        let bytes = document.as_bytes();
        let is_critical = |value: &[u8]| value.windows(8).any(|window| window == b"critical");

        let sequential = json_scan_values::<_, String>(bytes, usize::MAX - 1, is_critical);
        let parallel = json_scan_values::<_, String>(bytes, usize::MAX, is_critical);
        let prefiltered_sequential = json_scan_values_prefiltered::<_, String>(
            bytes,
            &Needle::new(b"critical", false),
            2,
            usize::MAX - 1,
            is_critical,
        );
        let prefiltered_parallel = json_scan_values_prefiltered::<_, String>(
            bytes,
            &Needle::new(b"critical", false),
            2,
            usize::MAX,
            is_critical,
        );
        let folded = json_fold_values_with_paths(bytes, Vec::new, |visited, path, start, end| {
            visited.push((path.to_vec(), start, end));
        });
        let mut visited = Vec::new();
        json_scan_values_with_paths(bytes, |path, start, end| {
            visited.push((path.to_vec(), start, end));
            true
        });

        assert!(!sequential.is_empty());
        assert_eq!(folded.len() > 1, document_index < 4);
        assert_eq!(parallel, sequential);
        assert_eq!(prefiltered_parallel, prefiltered_sequential);
        assert_eq!(folded.concat(), visited);
    }
}

/// The byte-at-a-time container skipper that the block skipper must reproduce.
fn reference_skip_container(bytes: &[u8], pos: usize, open: u8, close: u8) -> usize {
    let mut depth = 1_u32;
    let mut i = pos + 1;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'"' => {
                i = skip_escaped_string(bytes, i);
                continue;
            }
            b if b == open => depth += 1,
            b if b == close => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    i
}

#[test]
fn swar_string_skipping_matches_escape_aware_scanning() {
    const ALPHABET: &[u8] = b"ab\\\"";
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for case in 0..4_000 {
        let mut bytes = vec![b'"'];
        bytes.extend((0..case % 70).map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ALPHABET[(state % ALPHABET.len() as u64) as usize]
        }));
        let expected = skip_escaped_string(&bytes, 0);
        assert_eq!(
            super::bytes::skip_string(&bytes, 0),
            expected,
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(
            crate::yaml_scanner::scalar::quoted_scalar_end(&bytes, 0),
            expected
        );
    }
}

#[test]
fn block_container_skipping_matches_byte_scanning() {
    const DENSE: &[u8] = b"{}[]\"\\\"\"ab ,:";
    // Long strings with escape runs that cross block boundaries.
    let sparse = [&[b'a'; 120][..], b"\\\\\\\\\"{}[]"].concat();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for case in 0..6_000 {
        let (alphabet, len) = if case % 4 == 3 {
            (sparse.as_slice(), 1 + case % 1_200)
        } else {
            (DENSE, 1 + case % 400)
        };
        let mut bytes = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let byte = alphabet[(state % alphabet.len() as u64) as usize];
                // Half of the dense cases avoid backslashes.
                if case % 4 < 2 && byte == b'\\' {
                    b'a'
                } else {
                    byte
                }
            })
            .collect::<Vec<_>>();
        let (open, close) = if case % 2 == 0 {
            (b'{', b'}')
        } else {
            (b'[', b']')
        };
        bytes[0] = open;
        let unbounded = container_end_within(&bytes, 0, (open, close), len);
        let end = unbounded.unwrap_or(len);
        let limits = [
            (state % (len as u64 + 1)) as usize,
            end.saturating_sub(1),
            end,
            end + 1,
        ];

        let skipped = skip_container(&bytes, 0, open, close);
        let bounded = limits.map(|limit| container_end_within(&bytes, 0, (open, close), limit));

        let context = String::from_utf8_lossy(&bytes);
        assert_eq!(
            skipped,
            reference_skip_container(&bytes, 0, open, close),
            "{context:?}"
        );
        assert_eq!(
            bounded,
            limits.map(|limit| unbounded.filter(|&end| end <= limit)),
            "{limits:?} {context:?}"
        );
    }
}

#[test]
fn walks_that_read_no_path_leave_it_unbuilt_and_visit_the_same_scalars() {
    let bytes = br#"{"a":[true,{"b":-1.25}],"c":"d"}"#;
    let walk = |builds_paths: bool| {
        let mut spans = Vec::new();
        let mut longest_path = 0;
        let mut hooks = super::traversal::TraversalHooks {
            state: &mut (),
            visit: |_: &mut (), path: &[u8], start, end| {
                spans.push((start, end));
                longest_path = longest_path.max(path.len());
                true
            },
            prune: super::traversal::never_prune,
        };
        let mut path = vec![b'$'];
        let end = if builds_paths {
            super::traversal::scan_value_recursive::<false, true, _, _, _>(
                bytes, 0, 0, &mut path, &mut hooks,
            )
        } else {
            super::traversal::scan_value_recursive::<false, false, _, _, _>(
                bytes, 0, 0, &mut path, &mut hooks,
            )
        };
        (spans, end, longest_path)
    };

    let (spans, end, longest_path) = walk(true);
    let (pathless_spans, pathless_end, pathless_longest_path) = walk(false);

    assert_eq!(spans, [(6, 10), (16, 21), (28, 31)]);
    assert_eq!((pathless_spans, pathless_end), (spans, end));
    assert_eq!(end, bytes.len());
    assert_eq!(longest_path, "$.a[1].b".len());
    assert_eq!(pathless_longest_path, "$".len());
}

#[test]
fn scalar_visitor_preserves_paths_spans_and_stops_on_request() {
    let bytes = br#"{"a":[true,false,null,-1.25,"escaped\"text"],"b":{}}"#;
    let expected = [
        ("$.a[0]", "true"),
        ("$.a[1]", "false"),
        ("$.a[2]", "null"),
        ("$.a[3]", "-1.25"),
        ("$.a[4]", "\"escaped\\\"text\""),
    ];
    // A nonzero sentinel depth would overflow if plain recursion still incremented it.
    let mut plain_hooks = super::traversal::TraversalHooks {
        state: &mut (),
        visit: |_: &mut (), _: &[u8], _, _| true,
        prune: |_: &(), _, _, _| -> Option<usize> { panic!("plain traversal must not prune") },
    };
    let end = super::traversal::scan_value_recursive::<false, true, _, _, _>(
        bytes,
        0,
        usize::MAX,
        &mut vec![b'$'],
        &mut plain_hooks,
    );
    assert_eq!(end, bytes.len());
    for limit in 1..=expected.len() {
        let mut found = Vec::new();
        json_scan_values_with_paths(bytes, |path, start, end| {
            found.push((
                String::from_utf8(path.to_vec()).expect("UTF-8 path"),
                std::str::from_utf8(&bytes[start..end]).expect("UTF-8 value"),
            ));
            found.len() < limit
        });
        assert_eq!(
            found
                .iter()
                .map(|(path, value)| (path.as_str(), *value))
                .collect::<Vec<_>>(),
            expected[..limit]
        );
        assert_eq!(
            json_scan_values_prefiltered::<_, String>(
                bytes,
                &Needle::new(b"", false),
                0,
                limit,
                |_| true
            ),
            json_scan_values::<_, String>(bytes, limit, |_| true)
        );
    }
    assert!(
        json_scan_values::<_, String>(bytes, 0, |_| panic!("zero limit must not visit")).is_empty()
    );
    assert!(
        json_scan_values_prefiltered::<_, String>(
            bytes,
            &Needle::new(b"", false),
            0,
            0,
            |_| panic!("zero limit")
        )
        .is_empty()
    );
    let filtered = json_scan_values::<_, String>(bytes, 1, |value| value == b"null");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].path, "$.a[2]");
    for empty in [b"".as_slice(), b"{}", b"[]"] {
        json_scan_values_with_paths(empty, |_, _, _| panic!("no scalars"));
    }
}

#[test]
fn deep_containers_inside_split_array_elements_are_skipped() {
    let deep = format!("{}\"x\"{}", "[".repeat(2_000), "]".repeat(2_000));
    let mut elements = (0..30_000)
        .map(|index| format!(r#"{{"id":{index},"tag":"x"}}"#))
        .collect::<Vec<_>>();
    elements[15_000] = format!(r#"{{"id":-1,"tag":{deep}}}"#);
    let document = format!("[{}]", elements.join(","));
    assert!(document.len() > PARALLEL_SCAN_LEN);

    let plain =
        json_scan_values::<_, String>(document.as_bytes(), usize::MAX, |value| value == b"x");
    let prefiltered = json_scan_values_prefiltered::<_, String>(
        document.as_bytes(),
        &Needle::new(b"x", false),
        usize::MAX,
        usize::MAX,
        |value| value == b"x",
    );

    assert_eq!(plain.len(), 29_999);
    assert_eq!(prefiltered.len(), 29_999);
}

#[test]
fn deeply_nested_containers_are_skipped_without_unbounded_recursion() {
    // Deep enough to overflow a 2 MiB thread stack if every level recursed.
    let depth = 300_000;
    let mut document = "[".repeat(depth);
    document.push_str("\"x\"");
    document.push_str(&"]".repeat(depth));
    document = format!(r#"{{"shallow":"x","deep":{document}}}"#);
    assert_eq!(
        skip_container(document.as_bytes(), 0, b'{', b'}'),
        document.len()
    );

    let hits = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let document = document;
            let plain = json_scan_values::<_, String>(document.as_bytes(), usize::MAX, |value| {
                value == b"x"
            });
            let prefiltered = json_scan_values_prefiltered::<_, String>(
                document.as_bytes(),
                &Needle::new(b"x", false),
                // Never prune, so the walk itself must bound its recursion.
                usize::MAX,
                usize::MAX,
                |value| value == b"x",
            );
            (plain.len(), prefiltered.len())
        })
        .expect("thread spawns")
        .join()
        .expect("scan finishes without overflowing the stack");
    assert_eq!(hits, (1, 1));
}

#[test]
fn occurrence_pruned_scans_match_unpruned_scans() {
    // Large enough for parallel groups and several occurrence search windows.
    let records = (0..PARALLEL_SCAN_LEN / 25)
        .map(|index| {
            let rare = if index % 4000 == 7 {
                r#","rare":"Needle""#
            } else {
                ""
            };
            let (name, tag) = (index % 13, index % 7);
            format!(
                concat!(
                    r#"{{"id":{index},"name":"Svc-{name}","tags":["T{tag}"],"#,
                    r#""nested":{{"deep":[{{"v":"x"}}]}}{rare}}}"#,
                ),
                index = index,
                name = name,
                tag = tag,
                rare = rare
            )
        })
        .collect::<Vec<_>>();
    let document = format!(r#"{{"events":[{}],"after":"svc-1"}}"#, records.join(","));
    let bytes = document.as_bytes();
    // The first four occur in values; the rest are absent, key-only, or straddle a close.
    let needles = [
        &b"svc-1"[..],
        b"needle",
        b"x",
        b"",
        b"absent",
        b"tags",
        b"x\"}",
    ];
    for (needle_index, needle) in needles.into_iter().enumerate() {
        let contains = |value: &[u8]| memmem::find(&value.to_ascii_lowercase(), needle).is_some();
        let folding = Needle::new(needle, true);
        for (min_depth, limit) in [
            (0, usize::MAX),
            (2, usize::MAX),
            (0, 5),
            (2, usize::MAX - 1),
        ] {
            let expected = json_scan_values::<_, String>(bytes, limit, contains);

            let actual = json_scan_values_prefiltered::<_, String>(
                bytes, &folding, min_depth, limit, contains,
            );

            assert_eq!(actual, expected, "{needle:?} {min_depth} {limit}");
            assert_eq!(expected.is_empty(), needle_index >= 4, "{needle:?}");
        }
    }
}

#[test]
fn containers_are_kept_exactly_when_an_occurrence_lies_wholly_inside() {
    // The inner object closes exactly at the occurrence's end, then one byte before it. The
    // long filler pushes occurrences past the first 64 KiB search window.
    for filler in [String::new(), ".".repeat(70_000)] {
        let cases = [
            (
                format!(r#"{{"a":{{"v":"{filler}x"}}}}"#),
                &br#"x"}"#[..],
                "$.a.v",
            ),
            (
                format!(r#"{{"a":{{"v":"{filler}x"}},"b":"y"}}"#),
                br#"x"},"#,
                "$.b",
            ),
        ];
        for (document, needle, expected_path) in cases {
            let bytes = document.as_bytes();

            // Accepting every value exposes which containers pruning kept.
            let hits = json_scan_values_prefiltered::<_, String>(
                bytes,
                &Needle::new(needle, false),
                0,
                usize::MAX,
                |_| true,
            );

            let paths = hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>();
            assert_eq!(paths, [expected_path], "{needle:?} {}", filler.len());
        }
    }
}

#[test]
fn skippable_ends_reach_unterminated_and_short_containers() {
    // Unterminated containers (no needle, needle in the first window, needle beyond it) and
    // one that closes early, all followed by more than a search window.
    let filler = " ".repeat(3 * SEARCH_WINDOW);
    let opening = r#"{"v":""#;
    let documents = [
        (
            format!("{opening}{filler}"),
            Some(opening.len() + filler.len()),
        ),
        (format!("{opening}needle{filler}"), None),
        (format!("{opening}{filler}needle"), None),
        (format!(r#"{{"a":1}}{filler}"#), Some(7)),
    ];
    let finder = Needle::new(b"needle", false);
    for (document, expected) in documents {
        let bytes = document.as_bytes();
        let mut cursor = OccurrenceCursor::new(&finder, bytes);

        let skippable = cursor.skippable_end(0, |limit| {
            container_end_within(bytes, 0, (b'{', b'}'), limit)
        });

        assert_eq!(skippable, expected, "{}", &document[..12]);
    }
}

#[test]
fn a_fresh_prefiltered_state_collects_one_scan() {
    // Only the second object holds the needle, so the first is pruned.
    let bytes = br#"{"a":{"c":"x"},"b":{"c":"y"}}"#;
    let finder = Needle::new(b"y", false);
    let scan = PrefilteredSplitScan {
        bytes,
        needle: &finder,
        min_depth: 0,
        matcher: &|_: &[u8]| true,
        paths: std::marker::PhantomData::<fn() -> String>,
    };
    let mut path = b"$".to_vec();
    let mut state = scan.new_scan_state();
    let empty_at_start = state.hits.is_empty();

    let end = scan.scan_into::<false>(&mut state, (0, 0), &mut path, usize::MAX);

    let paths = state
        .hits
        .iter()
        .map(|hit| hit.path.as_str())
        .collect::<Vec<_>>();
    assert!(empty_at_start);
    assert_eq!(end, bytes.len());
    assert_eq!(paths, ["$.b.c"]);
    assert_eq!(path, b"$");
}

#[test]
fn path_patterns_read_back_every_printed_key() {
    let keys = |pattern: &str| {
        let query = parse_path_pattern(pattern.as_bytes(), 0).expect("valid path");
        let segments = query
            .segments
            .iter()
            .map(|segment| match segment {
                PathSegment::Key(key) => key.clone(),
                PathSegment::Index(index) => format!("#{index}"),
            })
            .collect::<Vec<_>>();
        (segments, query.expected_value)
    };

    assert_eq!(
        keys(r#"$.jobs["my-job"].steps[2].uses"#),
        (
            vec![
                "jobs".into(),
                "my-job".into(),
                "steps".into(),
                "#2".into(),
                "uses".into()
            ],
            None
        )
    );
    assert_eq!(
        keys(r#"$["say \"hi\""]=yes"#),
        (vec![r#"say "hi""#.into()], Some("yes".into()))
    );
    assert_eq!(
        keys(r#"$["a=b"].c=d=e"#),
        (vec!["a=b".into(), "c".into()], Some("d=e".into()))
    );
    assert!(parse_path_pattern(br#"$["open"#, 0).is_none());
    assert!(parse_path_pattern(b"$.flags[true]", 0).is_none());
}

/// The spans of a JSON stream's values, read in order; `None` for a single document. A value
/// that does not parse spans the rest of the stream, so parsing it reports the error.
pub(crate) fn json_value_stream(bytes: &[u8]) -> Option<Vec<(usize, usize)>> {
    let stream = json_stream_extent(bytes)?;
    let mut values = Vec::new();
    let mut next = stream.first.0;
    while next < stream.end {
        let (value, following) =
            json_stream_value(bytes, next, stream.end).unwrap_or(((next, stream.end), stream.end));
        values.push(value);
        next = following;
    }
    Some(values)
}

#[test]
fn short_stream_values_stop_reading_long_ones() {
    let stream = b"[1, 2]  {\"a\": [3]} 45 [";
    let end = stream.len();

    assert_eq!(
        json_short_stream_value(stream, 0, end, 7),
        Some(((0, 6), 8))
    );
    assert_eq!(json_short_stream_value(stream, 0, end, 6), None);
    assert_eq!(
        json_short_stream_value(stream, 8, end, 64),
        Some(((8, 18), 19))
    );
    assert_eq!(
        json_short_stream_value(stream, 19, end, 3),
        Some(((19, 21), 22))
    );
    assert_eq!(json_short_stream_value(stream, 19, end, 2), None);
    // Unterminated and unparseable values are not short ones.
    assert_eq!(json_short_stream_value(stream, 22, end, 64), None);
    assert_eq!(json_short_stream_value(b"1 :", 2, 3, 64), None);
}

#[test]
fn stream_values_report_their_span_and_the_next_value() {
    let stream = b"{\"a\":1}\n  [2,\n3] 4\n{\"cut\"";
    let end = stream.len();

    let extent = JsonStream { first: (0, 7), end };
    assert_eq!(json_stream_extent(stream), Some(extent));
    assert_eq!(json_stream_value(stream, 0, end), Some(((0, 7), 10)));
    assert_eq!(json_stream_value(stream, 10, end), Some(((10, 16), 17)));
    assert_eq!(json_stream_value(stream, 17, end), Some(((17, 18), 19)));
    // An unterminated container runs to the end of the stream.
    assert_eq!(json_stream_value(stream, 19, end), Some(((19, end), end)));
    assert_eq!(json_stream_value(b"1\n:2", 2, 4), None);
    // Trailing whitespace is not part of the stream.
    let trimmed = JsonStream {
        first: (1, 2),
        end: 4,
    };
    assert_eq!(json_stream_extent(b" 1\n2 \n"), Some(trimmed));
    assert_eq!(json_stream_value(b" 1\n2 \n", 3, 4), Some(((3, 4), 4)));
}

#[test]
fn line_value_starts_skip_to_the_next_line_with_content() {
    let stream = b"{\"a\":1}\n\n  {\"a\":2}\n \n";

    assert_eq!(json_line_value_start(stream, 0), Some(11));
    assert_eq!(json_line_value_start(stream, 7), Some(11));
    assert_eq!(json_line_value_start(stream, 8), Some(11));
    assert_eq!(json_line_value_start(stream, 11), None);
    assert_eq!(json_line_value_start(b"1 2", 0), None);
}

#[test]
fn small_documents_fold_into_a_reused_state_and_path() {
    let fold = |document: &str, path: &mut Vec<u8>, visited: &mut Vec<String>| {
        let visit = |visited: &mut Vec<String>, path: &[u8], start: usize, end: usize| {
            visited.push(format!(
                "{}={}",
                String::from_utf8_lossy(path),
                &document[start..end]
            ));
        };
        json_fold_small_document(document.as_bytes(), path, visited, &visit)
    };
    let mut path = b"left over".to_vec();
    let mut visited = Vec::new();

    let first = fold(" {\"a\":{\"b\":[1,\"x\"]}}", &mut path, &mut visited);
    let second = fold("[true]", &mut path, &mut visited);
    let large = format!("[{}1]", "1,".repeat(PARALLEL_SCAN_LEN / 2));
    let third = fold(&large, &mut path, &mut visited);

    assert!(first && second && !third);
    assert_eq!(visited, ["$.a.b[0]=1", "$.a.b[1]=\"x\"", "$[0]=true"]);
}

#[test]
fn value_streams_split_json_lines_and_leave_documents_whole() {
    let spans = |text: &str| {
        json_value_stream(text.as_bytes()).map(|spans| {
            spans
                .iter()
                .map(|&(start, end)| text[start..end].to_string())
                .collect::<Vec<_>>()
        })
    };

    assert_eq!(
        spans("{\"a\":1}\n{\"a\":[2,\n3]}\r\n\n  \"x\" 4\n"),
        Some(vec![
            "{\"a\":1}".into(),
            "{\"a\":[2,\n3]}".into(),
            "\"x\"".into(),
            "4".into()
        ])
    );
    assert_eq!(spans("[1]\n[2]"), Some(vec!["[1]".into(), "[2]".into()]));
    for document in [
        "{\"a\":1}",
        "{\"a\":1}\n",
        "{\n  \"a\": 1\n}\n",
        "[\n1,\n2\n]",
        "",
        "  \n",
    ] {
        assert_eq!(spans(document), None, "{document:?}");
    }
    // Long records hide the first line break from its probe, but not the last one.
    let long = format!("{{\"pad\":\"{}\"}}", "x".repeat(8192));
    assert_eq!(
        spans(&format!("{long}\n{{\"a\":1}}\n")).map(|values| values.len()),
        Some(2)
    );
    assert_eq!(
        spans(&format!("{{\"a\":1}}\n{long}\n")).map(|values| values.len()),
        Some(2)
    );
    assert_eq!(spans(&format!("{long}\n")), None);
    assert_eq!(spans(&format!("{{\n\"a\": {long}\n}}")), None);
    // An unparseable value is left to the parser, which reports it.
    assert_eq!(
        spans("{\"a\":1}\noops {\"b\":2}"),
        Some(vec!["{\"a\":1}".into(), "oops {\"b\":2}".into()])
    );
}

#[test]
fn path_navigation_matches_keys_by_their_decoded_text() {
    let document = br#"{"x":{"eA":1,"a\\\\b":2,"a\\b":3,"q\"":4}}"#;
    let cases: [(&str, Option<&[u8]>); 5] = [
        ("$.x.eA", Some(b"1")),
        (r#"$.x["a\\\\b"]"#, Some(b"2")),
        (r#"$.x["a\\b"]"#, Some(b"3")),
        (r#"$.x["q\""]"#, Some(b"4")),
        (r#"$.x["e\\u0041"]"#, None),
    ];
    let queries = cases
        .iter()
        .map(|(pattern, _)| parse_path_pattern(pattern.as_bytes(), 0).expect("valid path"))
        .collect::<Vec<_>>();

    let batched = JsonPathBatch::new(&queries).navigate(document);

    for ((pattern, expected), (query, batched)) in cases.iter().zip(queries.iter().zip(batched)) {
        let text = |span: Option<(usize, usize)>| span.map(|(start, end)| &document[start..end]);
        assert_eq!(
            text(json_navigate_path(document, &query.segments)),
            *expected,
            "{pattern}"
        );
        assert_eq!(text(batched), *expected, "{pattern} in a batch");
    }
}
