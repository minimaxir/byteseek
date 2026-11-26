use crate::occurrences::Needle;

use super::{
    PARALLEL_SCAN_LEN, TomlScanState, toml_fold_values_with_paths, toml_scan_values,
    toml_scan_values_prefiltered, toml_traverse_values,
};

fn mixed_document() -> String {
    let mut document = String::from("title = \"root\"\n");
    let mut index = 0;
    while document.len() < 4 * PARALLEL_SCAN_LEN {
        match index % 5 {
            0 => document.push_str(&format!(
                "[[events]]\nid = {index}\nname.first = \"n{index}\"\n"
            )),
            1 => document.push_str(&format!(
                "  [[other]]\n# comment {index}\nvalue = {index}\n"
            )),
            2 => document.push_str(&format!("[meta.size]\nlist-len = 2\ncount = {index}\n")),
            3 => document.push_str(&format!("[[events]]\nid = {index}\n\n")),
            _ => document.push_str(&format!("key = 'v{index}'\n")),
        }
        index += 1;
    }
    document
}

const SUPPORTED: &str = "the document uses only syntax the scanner models";

#[test]
fn chunked_scans_match_one_sequential_pass() {
    let document = mixed_document();
    let bytes = document.as_bytes();
    let mut expected = Vec::new();
    toml_traverse_values(
        bytes,
        0..bytes.len(),
        &mut TomlScanState::new(),
        |state, key, start, end| {
            expected.push((state.key_path(bytes, key).to_vec(), start, end));
            true
        },
    );

    // The limit is checked after each visited value, so even zero yields one hit.
    let scan = |limit| toml_scan_values::<_, String>(bytes, limit, |_| true).expect(SUPPORTED);
    assert_eq!(scan(0).len(), 1);
    let hits = scan(usize::MAX)
        .into_iter()
        .map(|hit| (hit.path.into_bytes(), hit.start, hit.end))
        .collect::<Vec<_>>();
    let folded = toml_fold_values_with_paths(bytes, Vec::new, |visited, path, start, end| {
        visited.push((path.to_vec(), start, end));
    })
    .expect(SUPPORTED);

    assert!(folded.len() > 1);
    assert!(
        expected
            .iter()
            .any(|(path, _, _)| path.starts_with(b"$.events[200]"))
    );
    assert_eq!(hits, expected);
    assert_eq!(folded.concat(), expected);
}

#[test]
fn occurrence_driven_scans_match_full_scans() {
    // Mixed-case values, searched case-insensitively as the fast path does.
    let document = mixed_document().replace("\"n", "\"N");
    let bytes = document.as_bytes();
    let as_tuples = |hits: Vec<super::TomlScalarHit>| {
        hits.into_iter()
            .map(|hit| (hit.path, hit.start, hit.end))
            .collect::<Vec<_>>()
    };
    // Needles in values (the first three), keys, headers, comments, nowhere, and across a
    // newline.
    let needles = [
        &b"v1"[..],
        b"n2",
        b"1",
        b"name",
        b"events",
        b"comment",
        b"absent",
        b"2\n",
    ];
    for (needle_index, needle) in needles.into_iter().enumerate() {
        let contains =
            |value: &[u8]| memchr::memmem::find(&value.to_ascii_lowercase(), needle).is_some();
        let folding = Needle::new(needle, true);
        for limit in [usize::MAX, 3, 1] {
            let expected =
                as_tuples(toml_scan_values::<_, String>(bytes, limit, contains).expect(SUPPORTED));

            let actual = as_tuples(
                toml_scan_values_prefiltered::<_, String>(bytes, &folding, limit, contains)
                    .expect(SUPPORTED),
            );

            assert_eq!(actual, expected, "{needle:?} {limit}");
            assert_eq!(expected.is_empty(), needle_index >= 3, "{needle:?} {limit}");
        }
    }
}

#[test]
fn syntax_beyond_plain_key_lines_needs_the_parser() {
    let unsupported = [
        "user = {vip = true}\n",
        "ports = [80, 443]\n",
        "ports = [\n  80,\n]\n",
        "\"a.b\" = 1\n",
        "a . b = 1\n",
        "text = \"\"\"\nkey = 5\"\"\"\n",
        "text = '''\nkey = 5'''\n",
        "[\"quoted\"]\nx = 1\n",
        "[[events]]\nid = 1\n[events.user]\nvip = true\n",
        "[[a]]\n[[a.b]]\ny = 3\n",
    ];
    for document in unsupported {
        let bytes = document.as_bytes();

        let scanned = toml_scan_values::<_, String>(bytes, usize::MAX, |_| true);
        let folded = toml_fold_values_with_paths(bytes, || (), |(), _, _, _| ());

        assert!(scanned.is_none(), "{document:?}");
        assert!(folded.is_none(), "{document:?}");
    }
    let supported = "[a]\nx = 1\n[a.b]\ny-z = 'q'\n[[c]]\nd.e = 2\n";
    assert!(toml_scan_values::<_, String>(supported.as_bytes(), usize::MAX, |_| true).is_some());
}

#[test]
fn prefiltered_scans_defer_when_a_skipped_line_opens_a_multiline_string() {
    let document = "a = \"x\"\ntext = \"\"\"\nkey = \"x\"\n\"\"\"\nb = \"x\"\n";
    let bytes = document.as_bytes();
    let needle = Needle::new(b"x", false);
    let scan = |limit| {
        toml_scan_values_prefiltered::<_, String>(bytes, &needle, limit, |_| true)
            .map(|hits| hits.into_iter().map(|hit| hit.path).collect::<Vec<_>>())
    };

    assert_eq!(scan(usize::MAX), None);
    // Stopping at the first hit leaves the string after it unread.
    assert_eq!(scan(1), Some(vec!["$.a".to_owned()]));
}
