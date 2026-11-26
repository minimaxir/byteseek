use std::mem::size_of;

use super::parallel::{PARALLEL_SCAN_LEN, YamlChunkPlan, root_line_before};
use super::*;

#[test]
fn yaml_traversal_variants_preserve_paths_and_offsets() {
    assert_eq!(size_of::<LeafKey>(), 2 * size_of::<usize>());
    let yaml = b"events:\n  - name: \"svc-000\"\n  - name: \"svc-001\"\n";
    let hits = yaml_scan_values::<_, String>(yaml, usize::MAX, |value| value == b"svc-001")
        .expect("supported YAML should use the scanner");
    let filtered = yaml_scan_values_prefiltered::<_, String>(
        yaml,
        &Needle::new(b"svc-001", false),
        usize::MAX,
        |value| value == b"svc-001",
    )
    .expect("supported YAML should use the prefiltered scanner");
    let paths = yaml_fold_values_with_paths(yaml, None, Vec::new, |paths, path, start, end| {
        if &yaml[start..end] == b"\"svc-001\"" {
            paths.push((path.to_vec(), start, end));
        }
    })
    .expect("supported YAML should report paths")
    .concat();

    for limit in [0, 1] {
        let limited = yaml_scan_values::<_, String>(yaml, limit, |_| true).expect("supported YAML");
        assert_eq!(limited.len(), limit);
    }
    assert_eq!(hits.len(), 1);
    assert_eq!(filtered.len(), 1);
    assert_eq!(paths.len(), 1);
    assert_eq!(hits[0].path, "$.events[1].name");
    assert_eq!(filtered, hits);
    assert_eq!(
        paths[0],
        (hits[0].path.as_bytes().to_vec(), hits[0].start, hits[0].end)
    );
    let scalars = b": a\nseq:\n  - b\n";
    let edge_hits =
        yaml_scan_values::<_, String>(scalars, usize::MAX, |_| true).expect("supported YAML");
    assert_eq!(
        edge_hits
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        [r#"$[""]"#, "$.seq[0]"]
    );
}

#[test]
fn yaml_traversal_variants_fall_back_for_unsupported_syntax() {
    let yaml = b"value:\n  [one, two]\n";

    assert!(yaml_scan_values::<_, String>(yaml, usize::MAX, |_| true).is_none());
    assert!(yaml_scan_values::<_, String>(yaml, 1, |_| true).is_none());
    assert!(yaml_fold_values_with_paths(yaml, None, || (), |_, _, _, _| {}).is_none());
    assert!(
        yaml_scan_values_prefiltered::<_, String>(
            yaml,
            &Needle::new(b"one", false),
            usize::MAX,
            |_| true
        )
        .is_none()
    );
}

#[test]
fn quoted_scalars_only_the_parser_reads_fall_back() {
    let on_one_line = b"name: \"svc http\"\nitems:\n  - 'a b'\n  - c: 1\n    d: 2\nlast: 3\n";
    let unsupported: [&[u8]; 9] = [
        b"name: \"svc\n  http\"\nport: 1\n",
        b"name: 'svc\n  http'\nport: 1\n",
        b"items:\n  - \"a\n    b\"\n",
        b"name: \"svc\n",
        b"name: \"svc",
        b"items:\n  - 'svc",
        b"name: \"s\\x76c\"\n",
        b"items:\n  - 'it''s'\n",
        // A tab is no indentation.
        b"a: 1\n\tb: 2\n",
    ];

    let hits = sequential_hits(on_one_line);
    let fallbacks = unsupported.map(|yaml| sequential_hits(yaml).is_none());

    assert_eq!(hits.map(|hits| hits.len()), Some(5));
    assert_eq!(fallbacks, [true; 9]);
}

type PathHit = (Vec<u8>, usize, usize);

fn sequential_hits(yaml: &[u8]) -> Option<Vec<PathHit>> {
    let mut hits = Vec::new();
    yaml_traverse_values::<true, _, _, _>(
        yaml,
        0..yaml.len(),
        &mut YamlScanState::new(),
        |path, key, start, end| {
            with_leaf_path(path, (yaml, key), |path| {
                hits.push((path.to_vec(), start, end))
            });
        },
        |_, _, _| None,
        || false,
    )?;
    Some(hits)
}

fn chunked_hits(yaml: &[u8]) -> Option<Vec<Vec<PathHit>>> {
    yaml_fold_values_with_paths(yaml, None, Vec::new, |hits, path, start, end| {
        hits.push((path.to_vec(), start, end));
    })
}

fn event_items(count: usize, indent: &str) -> String {
    (0..count)
        .map(|index| {
            format!(
                "{indent}- id: {index}\n{indent}  service:\n{indent}    name: svc-{index}\n\
                 {indent}    tags:\n{indent}      - t{index}\n{indent}      - \"q: {index}\"\n\
                 {indent}  status: ok\n"
            )
        })
        .collect()
}

/// Root-level sequences of items at `indent`, the first one short.
fn sectioned_document(count: usize, indent: &str) -> String {
    format!(
        "meta:\n  format: 2\ngrids:\n{}# entities follow\n\nentities:\n{}tail:\n{}",
        event_items(3, indent),
        event_items(count, indent),
        event_items(count / 2, indent)
    )
}

#[test]
fn root_lines_are_found_past_blank_and_comment_lines() {
    let yaml = b"a:\n# note\n\n   \n- x\n  b:\n  - y\n- z\n";
    let line = |text: &[u8]| memchr::memmem::find(yaml, text).expect("line is present");

    assert_eq!(root_line_before(yaml, line(b"- x")), Some(0));
    assert_eq!(root_line_before(yaml, line(b"  - y")), None);
    assert_eq!(root_line_before(yaml, line(b"- z")), None);
    assert_eq!(root_line_before(b"- x\n- y\n", 4), None);
    assert_eq!(root_line_before(b"- x\n", 0), None);
}

#[test]
fn chunk_plans_assume_true_states_across_root_level_sequences() {
    for indent in ["", "  "] {
        let document = sectioned_document(PARALLEL_SCAN_LEN / 50, indent);
        let bytes = document.as_bytes();
        let plan = YamlChunkPlan::new(bytes).expect("large documents with sequences split");

        assert!(plan.starts.len() > 2);
        for chunk in 1..plan.starts.len() {
            let mut state = YamlScanState::new();
            yaml_traverse_values::<true, _, _, _>(
                bytes,
                0..plan.starts[chunk],
                &mut state,
                |_, _, _, _| {},
                |_, _, _| None,
                || false,
            );
            assert!(
                plan.leads_into(chunk, &state),
                "chunk {chunk}, indent {indent:?}"
            );
        }
    }
}

#[test]
fn chunked_traversals_match_one_sequential_traversal() {
    let count = PARALLEL_SCAN_LEN / 50;
    let events = format!("# log\nevents:\n{}", event_items(count, "  "));
    let documents = [
        events.clone(),
        event_items(count, ""),
        format!("{events}other:\n{}", event_items(count, "  ")),
        sectioned_document(count, ""),
        sectioned_document(count, "  "),
        // Chunks in `b` wrongly assume they continue `a` and must be rescanned.
        format!(
            "root:\n  a:\n{}  b:\n{}top:\n{}",
            event_items(count / 2, "  "),
            event_items(count / 2, "  "),
            event_items(count, "  ")
        ),
    ];

    for (index, document) in documents.iter().enumerate() {
        let chunks = chunked_hits(document.as_bytes()).expect("supported YAML");

        assert!(chunks.len() > 1, "document {index} should be split");
        assert_eq!(Some(chunks.concat()), sequential_hits(document.as_bytes()));
    }
}

#[test]
fn chunked_traversals_reject_late_unsupported_syntax() {
    let yaml = format!(
        "events:\n{}tail:\n  [1, 2]\n",
        event_items(PARALLEL_SCAN_LEN / 50, "  ")
    );

    assert!(sequential_hits(yaml.as_bytes()).is_none());
    assert!(chunked_hits(yaml.as_bytes()).is_none());
}

#[test]
fn chunked_prefiltered_scans_match_unfiltered_scans() {
    let yaml = format!("events:\n{}", event_items(PARALLEL_SCAN_LEN / 50, "  "));
    let bytes = yaml.as_bytes();
    let wanted = |value: &[u8]| value.ends_with(b"7");

    let filtered = yaml_scan_values_prefiltered::<_, String>(
        bytes,
        &Needle::new(b"7", false),
        usize::MAX,
        wanted,
    )
    .expect("supported YAML");
    let unfiltered =
        yaml_scan_values::<_, String>(bytes, usize::MAX, wanted).expect("supported YAML");

    assert!(!filtered.is_empty());
    assert_eq!(filtered, unfiltered);
}

/// Line-by-line reference for [`find_section_end`].
fn section_end_by_lines(bytes: &[u8], start: usize, max_indent: usize) -> Option<usize> {
    let mut position = next_line(bytes, start);
    while position < bytes.len() {
        let content = skip_spaces(bytes, position);
        if content < bytes.len()
            && !matches!(bytes[content], b'\n' | b'#')
            && content - position <= max_indent
        {
            return Some(position);
        }
        position = next_line(bytes, content);
    }
    None
}

#[test]
fn block_tested_section_ends_match_a_line_walk() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    for _ in 0..60 {
        // Random indentation, blanks, comments, tab-and-CR content, and long deep runs.
        let mut document = Vec::new();
        for _ in 0..next(80) {
            let indent = if next(4) == 0 { next(4) } else { 4 + next(14) };
            document.extend(std::iter::repeat_n(b' ', indent));
            match next(10) {
                0 => {}
                1 => document.extend_from_slice(b"# note"),
                2 => document.extend_from_slice(b"\tx\r"),
                _ => document.extend(std::iter::repeat_n(b'x', 1 + next(90))),
            }
            document.push(b'\n');
        }
        if next(2) == 0 {
            document.pop();
        }
        for start in 0..document.len() {
            for max_indent in [0, 1, 2, 3, 5, 8, 12, 17] {
                let expected = section_end_by_lines(&document, start, max_indent);
                let limit = if next(2) == 0 {
                    next(document.len() + 1)
                } else {
                    expected.unwrap_or(document.len()).saturating_sub(1) + next(3)
                };

                let unbounded =
                    find_section_end::<64>(&document, start, max_indent, document.len());
                let bounded = find_section_end::<64>(&document, start, max_indent, limit);

                assert_eq!(unbounded, expected, "{start} {max_indent} {document:?}");
                assert_eq!(
                    bounded,
                    expected.filter(|&end| end <= limit),
                    "{start} {max_indent} {limit} {document:?}"
                );
            }
        }
    }
}

#[test]
fn occurrence_skipped_items_match_full_scans() {
    // Mixed-case items, long enough for parallel chunks, searched case-insensitively.
    let mut document = String::from("events:\n");
    let mut index = 0;
    while document.len() < 3 * PARALLEL_SCAN_LEN {
        document.push_str(&format!("  - id: {index}\n    name: N{}\n", index % 13));
        if index % 3 == 0 {
            document.push_str(&format!(
                "    tags:\n      - t{}\n\n    # c{index}\n",
                index % 7
            ));
        }
        index += 1;
    }
    document.push_str("after: n1\n");
    let bytes = document.as_bytes();
    // The first three occur in values; the rest are in keys, comments, nowhere, or across
    // item boundaries.
    let needles = [
        &b"n1"[..],
        b"t3",
        b"",
        b"tags",
        b"c4",
        b"absent",
        b"n2\n  - id",
    ];
    for (needle_index, needle) in needles.into_iter().enumerate() {
        let contains =
            |value: &[u8]| memchr::memmem::find(&value.to_ascii_lowercase(), needle).is_some();
        let folding = Needle::new(needle, true);
        for limit in [usize::MAX, 5, 1] {
            let as_tuples = |hits: Option<Vec<YamlScalarHit>>| {
                hits.expect("block YAML is supported")
                    .into_iter()
                    .map(|hit| (hit.path, hit.start, hit.end))
                    .collect::<Vec<_>>()
            };
            let expected = as_tuples(yaml_scan_values::<_, String>(bytes, limit, contains));

            let actual = as_tuples(yaml_scan_values_prefiltered::<_, String>(
                bytes, &folding, limit, contains,
            ));

            assert_eq!(actual, expected, "{needle:?} {limit}");
            assert_eq!(expected.is_empty(), needle_index >= 3, "{needle:?} {limit}");
        }
    }
}

#[test]
fn sequence_items_are_skipped_only_without_an_occurrence() {
    // The long filler pushes occurrences past the first 64 KiB search window.
    for filler in [String::new(), ".".repeat(70_000)] {
        let document = format!("- a\n- b \"{filler}x\n  y\"\n- c\n");
        let bytes = document.as_bytes();
        let second_item = 4;
        let third_item = bytes.len() - 4;
        let skipped = |needle: &str, item_start: usize| {
            let finder = Needle::new(needle.as_bytes(), false);
            let mut cursor = OccurrenceCursor::new(&finder, bytes);
            skipped_item_end(&mut cursor, bytes, (item_start, item_start + 2), 0)
        };

        let before_occurrence = skipped("c", 0);
        let holding_occurrence = skipped("x\n  y", second_item);
        let ending_at_item_end = skipped("y\"\n", second_item);
        let one_past_item_end = skipped("y\"\n-", second_item);
        let last_without_occurrence = skipped("q", third_item);
        let empty_needle = skipped("", 0);

        let context = filler.len();
        assert_eq!(before_occurrence, Some(second_item), "{context}");
        assert_eq!(holding_occurrence, None, "{context}");
        assert_eq!(ending_at_item_end, None, "{context}");
        assert_eq!(one_past_item_end, Some(third_item), "{context}");
        assert_eq!(last_without_occurrence, Some(bytes.len()), "{context}");
        assert_eq!(empty_needle, None, "{context}");
    }
}

#[test]
fn plain_scalars_end_before_trailing_comments_only() {
    let end = |line: &str| plain_scalar_end(line.as_bytes(), 0, line.len());

    assert_eq!(end("failed # note"), 7);
    assert_eq!(end("failed\t# note"), 7);
    assert_eq!(end("a#b # c"), 4);
    assert_eq!(end("http://host/#anchor"), 19);
    assert_eq!(end("# starts the scalar"), 19);
    assert_eq!(end(""), 0);
}

#[test]
fn values_only_the_full_parser_reads_are_recognized() {
    for value in [
        "[a]",
        "{a: 1}",
        "|",
        ">-",
        "&anchor v",
        "!tag v",
        "*alias",
        "  [a]",
    ] {
        assert!(starts_unsupported_value(value.as_bytes(), 0), "{value}");
    }
    for value in ["plain", "\"[quoted]\"", "'*'", "-1", "a[0]"] {
        assert!(!starts_unsupported_value(value.as_bytes(), 0), "{value}");
    }
}

#[test]
fn spaced_values_keep_their_quotes_and_spaced_items_fall_back() {
    let yaml = b"msg:  \"a # b\"\nplain:   c # d\nempty:   # only a comment\n  nested: e\n";
    let all_values =
        |document: &[u8]| yaml_scan_values::<_, String>(document, usize::MAX, |_| true);

    let hits = all_values(yaml).expect("spaces before a value are supported");
    let spaced_scalar_item = all_values(b"-  \"x # y\"\n");
    let spaced_mapping_item = all_values(b"-   key: v\n    other: w\n");

    assert_eq!(
        hits.iter()
            .map(|hit| (hit.path.as_str(), &yaml[hit.start..hit.end]))
            .collect::<Vec<_>>(),
        [
            ("$.msg", &b"\"a # b\""[..]),
            ("$.plain", b"c"),
            ("$.empty.nested", b"e")
        ]
    );
    assert!(spaced_scalar_item.is_none());
    assert!(spaced_mapping_item.is_none());
}

#[test]
fn folds_skip_items_without_the_prefilter_unless_they_hold_an_alias() {
    let yaml = b"events:\n  - name: keep\n    level: 1\n  - name: drop\n    level: 2\nlast: keep\n";
    let needle = Needle::new(b"keep", false);
    let values = |document: &[u8], prefilter: Option<&Needle>| {
        yaml_fold_values_with_paths(document, prefilter, Vec::new, |values, path, start, end| {
            values.push((path.to_vec(), start, end));
        })
        .map(|chunks| chunks.concat())
    };

    let all = values(yaml, None).expect("block YAML is supported");
    let kept = values(yaml, Some(&needle)).expect("block YAML is supported");
    let aliased = values(b"events:\n  - name: *shared\n", Some(&needle));
    let without_alias = values(b"events:\n  - name: other\n", Some(&needle));

    let outside_dropped_item: Vec<_> = all
        .iter()
        .filter(|(path, ..)| !path.starts_with(b"$.events[1]"))
        .cloned()
        .collect();
    assert_eq!(all.len(), 5);
    assert_eq!(kept, outside_dropped_item);
    assert_eq!(kept.len(), 3);
    assert!(aliased.is_none());
    assert_eq!(without_alias, Some(Vec::new()));
}

#[test]
fn a_sequence_item_cut_off_after_its_dash_scans_as_empty() {
    for yaml in ["- ", "a: 1\n- ", "items:\n  - ", "items:\n  - a\n  - "] {
        let hits = yaml_scan_values::<_, String>(yaml.as_bytes(), usize::MAX, |_| true);
        let through_api = crate::Searcher::new(
            ["zzz"],
            crate::SearchConfig::builder()
                .structured_mode(Some(crate::StructuredMode::Yaml))
                .build(),
        )
        .expect("a value pattern compiles")
        .try_find(yaml);

        assert!(hits.is_none_or(|hits| hits.iter().all(|hit| hit.start <= hit.end)));
        assert_eq!(through_api, Ok(Vec::new()), "{yaml:?}");
    }
}
