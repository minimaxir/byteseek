//! Structured searches over realistic documents that defeat speculation tuned to uniform event
//! lists: escape-heavy JSON, nested JSON objects sharing a leading key, and YAML with several
//! root-level sequences.

use std::fmt::Write;
use std::hint::black_box;
use std::time::Duration;

use byteseek::{CaseMode, SearchConfig, Searcher, StructuredMode};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "support/variants.rs"]
mod variants;
use variants::bench_independent_variants;

const VARIANT_COUNT: usize = 64;
const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";
const WORDS: [&str; 16] = [
    "font", "glyph", "outline", "render", "bitmap", "face", "size", "metrics", "kerning",
    "hinting", "library", "driver", "module", "stream", "cache", "charmap",
];
const ENTITY_TYPES: [&str; 5] = [
    "Catwalk",
    "DisposalPipe",
    "Poweredlight",
    "solid_wall",
    "Table",
];

/// Deterministic xorshift stream for synthetic text.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }
}

/// A documentation search index: a few root keys and an array of pages whose long prose
/// strings carry `\u` and `\"` escapes.
fn string_heavy_json(pages: usize) -> String {
    let mut random = Xorshift(0x9e37_79b9_7f4a_7c15);
    let mut json = String::from(
        r#"{"config":{"indexing":"full","lang":["en"],"separator":"[\\s\\-]+"},"docs":["#,
    );
    for page in 0..pages {
        if page != 0 {
            json.push(',');
        }
        write!(json, r#"{{"location":"page-{page}.html","text":""#).expect("String writes");
        for word in 0..160 {
            match random.next() % 48 {
                0 => json.push_str(r"» "),
                1 => json.push_str(r#"\"quoted\" "#),
                _ => {}
            }
            json.push_str(WORDS[random.next() % WORDS.len()]);
            json.push(if word % 12 == 11 { '.' } else { ' ' });
        }
        write!(json, r#"","title":"Section {page}"}}"#).expect("String writes");
    }
    json.push_str("]}");
    json
}

/// Cargo metadata: package records led by `"name"`, whose dependency and resolve entries are
/// objects led by `"name"` too.
fn package_metadata_json(packages: usize) -> String {
    let mut json = String::from(r#"{"packages":["#);
    for package in 0..packages {
        if package != 0 {
            json.push(',');
        }
        write!(
            json,
            r#"{{"name":"crate-{package}","version":"1.0.{package}","id":"{REGISTRY}#crate-{package}@1.0.{package}","license":"MIT OR Apache-2.0","source":"{REGISTRY}","dependencies":["#
        )
        .expect("String writes");
        for dependency in 0..9 {
            if dependency != 0 {
                json.push(',');
            }
            write!(
                json,
                r#"{{"name":"dep-{dependency}","source":"{REGISTRY}","req":"^0.{dependency}","kind":null,"optional":false,"features":[]}}"#
            )
            .expect("String writes");
        }
        write!(
            json,
            r#"],"targets":[{{"kind":["lib"],"crate_types":["lib"],"name":"crate_{package}","src_path":"/home/user/.cargo/registry/src/crate-{package}/src/lib.rs","edition":"2021"}}],"features":{{"default":["std"],"std":[]}}}}"#
        )
        .expect("String writes");
    }
    json.push_str(r#"],"resolve":{"nodes":["#);
    for package in 0..packages {
        if package != 0 {
            json.push(',');
        }
        write!(
            json,
            r#"{{"id":"{REGISTRY}#crate-{package}@1.0.{package}","deps":[{{"name":"dep_1","pkg":"{REGISTRY}#dep-1@0.1.0"}},{{"name":"dep_2","pkg":"{REGISTRY}#dep-2@0.2.0"}}],"features":["default","std"]}}"#
        )
        .expect("String writes");
    }
    json.push_str(r#"],"root":null},"version":1}"#);
    json
}

/// A map file: root-level metadata, a short `grids` sequence, then a long `entities`
/// sequence, both of unindented items holding nested sequences.
fn sectioned_yaml(entities: usize) -> String {
    let mut yaml = String::from("meta:\n  format: 2\n  name: DemoStation\ntilemap:\n");
    for tile in 0..64 {
        writeln!(yaml, "  {tile}: floor_{tile}").expect("String writes");
    }
    yaml.push_str("grids:\n- settings:\n    chunksize: 16\n  chunks:\n");
    for chunk in 0..24 {
        writeln!(
            yaml,
            "  - ind: \"{chunk},-1\"\n    tiles: {}",
            "OgAAADoAAAA".repeat(24)
        )
        .expect("String writes");
    }
    yaml.push_str("entities:\n");
    for uid in 0..entities {
        write!(
            yaml,
            "- uid: {uid}\n  type: {}\n  components:\n  - parent: 15\n    pos: {}.5,-14.5\n    \
             type: Transform\n",
            ENTITY_TYPES[uid % ENTITY_TYPES.len()],
            uid % 64
        )
        .expect("String writes");
        if uid % 3 == 0 {
            yaml.push_str("  - containers:\n      DisposalEntry:\n        type: Container\n");
        }
    }
    yaml
}

/// Copies of `base` that differ only in trailing whitespace or a trailing comment.
fn variants_of(base: &str, mode: StructuredMode) -> Vec<String> {
    (0..VARIANT_COUNT)
        .map(|index| match mode {
            StructuredMode::Json => format!("{base}\n{}", " ".repeat(index)),
            StructuredMode::Yaml | StructuredMode::Toml => {
                format!("{base}\n# benchmark variant {index}\n")
            }
        })
        .collect()
}

fn benchmark_generalization(c: &mut Criterion) {
    // Every document is over 512 KiB, large enough to be scanned in parallel.
    let documents = [
        (
            "json_string_heavy",
            StructuredMode::Json,
            string_heavy_json(800),
        ),
        (
            "json_nested_keys",
            StructuredMode::Json,
            package_metadata_json(300),
        ),
        ("yaml_sections", StructuredMode::Yaml, sectioned_yaml(8_000)),
    ];
    // (pattern, case mode, expected match count)
    let cases = [
        (
            "json_string_heavy",
            "path_value_miss",
            r#"$.location="page-7.html""#,
            CaseMode::Sensitive,
            0,
        ),
        (
            "json_string_heavy",
            "value",
            "kerning",
            CaseMode::AsciiInsensitive,
            800,
        ),
        (
            "json_string_heavy",
            "predicate",
            r#"$.location="page-7.html" AND $.title="Section 7""#,
            CaseMode::Sensitive,
            1,
        ),
        (
            "json_nested_keys",
            "value",
            "apache",
            CaseMode::AsciiInsensitive,
            300,
        ),
        (
            "json_nested_keys",
            "predicate",
            r#"$.name="crate-57" AND $.version="1.0.57""#,
            CaseMode::Sensitive,
            1,
        ),
        (
            "yaml_sections",
            "value",
            "wall",
            CaseMode::AsciiInsensitive,
            1_600,
        ),
        (
            "yaml_sections",
            "predicate",
            r#"$.uid=1263 AND $.type="solid_wall""#,
            CaseMode::Sensitive,
            1,
        ),
    ];

    let mut group = c.benchmark_group("generalization");
    for (document_name, mode, document) in &documents {
        let haystacks = variants_of(document, *mode);
        group.throughput(Throughput::Bytes(document.len() as u64));
        for &(_, query, pattern, case_mode, expected) in
            cases.iter().filter(|case| case.0 == *document_name)
        {
            let searcher = Searcher::new(
                [pattern],
                SearchConfig::builder()
                    .case_mode(case_mode)
                    .structured_mode(Some(*mode))
                    .build(),
            )
            .expect("benchmark patterns are valid");
            let found = searcher
                .find_structured(&haystacks[0])
                .expect("benchmark documents are valid");
            assert_eq!(found.len(), expected, "{document_name}/{query}");

            group.bench_with_input(
                BenchmarkId::new(*document_name, query),
                &haystacks,
                |bench, haystacks| {
                    bench_independent_variants(bench, haystacks, |text| {
                        black_box(searcher.find_structured(text).ok());
                    });
                },
            );
        }
    }
    group.finish();
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(4))
        .sample_size(30)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_generalization
);
criterion_main!(benches);
