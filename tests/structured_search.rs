use byteseek::{
    CaseMode, SearchConfig, SearchError, Searcher, StructuredMatchKind, StructuredMode,
    StructuredPredicateHandling,
};

fn build_structured_searcher(patterns: &[&str], config: SearchConfig) -> Searcher {
    Searcher::new(patterns.iter().copied(), config).expect("valid structured patterns")
}

fn structured_config(mode: StructuredMode) -> SearchConfig {
    SearchConfig::builder().structured_mode(Some(mode)).build()
}

#[test]
fn structured_json_batched_paths_preserve_query_order_and_limits() {
    let patterns = [
        "$.rows[3].v",
        "$.rows[0].v",
        "$.missing",
        "$.rows[1]",
        "$.rows[2].v",
        "$.rows",
        "$.rows[3].v",
        "$.rows[0]",
        "$.rows[99]",
        "$.rows[1].v",
    ];
    let document = r#"{"rows":[{"v":0},{"v":1},{"v":2},{"v":3}]}"#;
    let expected = patterns
        .iter()
        .enumerate()
        .flat_map(|(index, pattern)| {
            let single =
                build_structured_searcher(&[pattern], structured_config(StructuredMode::Json));
            single
                .find_structured(document)
                .expect("valid JSON")
                .into_iter()
                .map(move |mut found| {
                    found.pattern_index = index;
                    found
                })
        })
        .collect::<Vec<_>>();
    for limit in [0, 1, 7, 8, 9, usize::MAX] {
        let searcher = build_structured_searcher(
            &patterns,
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .max_matches(Some(limit))
                .build(),
        );
        assert_eq!(
            searcher.find_structured(document).expect("valid JSON"),
            expected[..limit.min(expected.len())],
            "limit={limit}"
        );
    }
}

#[test]
fn structured_json_path_matches_return_original_node_spans() {
    let searcher =
        build_structured_searcher(&["$.user.name"], structured_config(StructuredMode::Json));
    let haystack = r#"{"user":{"name":"Alice","age":42},"items":[{"name":"bolt"}]}"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse JSON");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "$.user.name");
    assert_eq!(matches[0].node_text, "\"Alice\"");
    assert_eq!(matches[0].match_kind, StructuredMatchKind::Path);
    assert_eq!(&haystack[matches[0].start..matches[0].end], "\"Alice\"");

    let plain = searcher
        .try_find(haystack)
        .expect("plain match projection should succeed");
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].start, matches[0].start);
    assert_eq!(plain[0].end, matches[0].end);
}

#[test]
fn structured_json_value_and_path_value_matching_work_for_scalars() {
    let searcher = build_structured_searcher(
        &["Alice", "$.items[1][\"display name\"]=nut"],
        structured_config(StructuredMode::Json),
    );
    let haystack = r#"{
  "user": {"name": "Alice"},
  "items": [
    {"display name": "bolt"},
    {"display name": "nut"}
  ]
}"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse JSON");
    let projection = matches
        .iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path.as_str(),
                matched.node_text,
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            (0, "$.user.name", "\"Alice\"", StructuredMatchKind::Value),
            (
                1,
                "$.items[1][\"display name\"]",
                "\"nut\"",
                StructuredMatchKind::PathValue,
            ),
        ]
    );
}

#[test]
fn structured_json_matching_respects_ascii_case_for_scalar_values() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .structured_mode(Some(StructuredMode::Json))
        .build();
    let searcher = build_structured_searcher(&["alice"], config);
    let haystack = r#"{"user":{"name":"Alice"},"other":"ALICE"}"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse JSON");
    let matched_paths = matches
        .iter()
        .map(|matched| matched.path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(matched_paths, vec!["$.user.name", "$.other"]);
}

#[test]
fn structured_limits_preserve_order_and_do_not_poison_broader_cache_reads() {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .build();
    let searcher = build_structured_searcher(&["$.a", "$.b", "$.c"], config);
    let haystack = r#"{"a":1,"b":2,"c":3}"#;

    let first = searcher
        .try_find_first(haystack)
        .expect("bounded structured search should parse")
        .expect("first path should match");
    assert_eq!(first.pattern_index, 0);

    let all = searcher
        .find_structured(haystack)
        .expect("broader search should not reuse an incomplete cache entry");
    assert_eq!(
        all.iter()
            .map(|matched| matched.pattern_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn structured_value_limits_do_not_poison_broader_cache_reads() {
    let cases = [
        (StructuredMode::Json, r#"{"a":"hit","b":"hit","c":"hit"}"#),
        (StructuredMode::Yaml, "a: hit\nb: hit\nc: hit\n"),
        (
            StructuredMode::Toml,
            "a = \"hit\"\nb = \"hit\"\nc = \"hit\"\n",
        ),
    ];

    for (mode, haystack) in cases {
        let searcher = build_structured_searcher(&["hit"], structured_config(mode));
        assert!(
            searcher
                .try_find_first(haystack)
                .expect("bounded value search should parse")
                .is_some()
        );

        let all = searcher
            .find_structured(haystack)
            .expect("broader value search should bypass a partial cache entry");
        assert_eq!(all.len(), 3, "mode {mode:?}");
    }
}

#[test]
fn structured_max_matches_bounds_mixed_query_results() {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .max_matches(Some(1))
        .build();
    let searcher = build_structured_searcher(&["$.events[0].status", "failed"], config);
    let haystack = r#"{"events":[{"status":"failed"},{"status":"failed"}]}"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("mixed structured search should parse");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].pattern_index, 0);
    assert_eq!(matches[0].path, "$.events[0].status");
}

#[test]
fn strict_predicate_errors_remain_visible_after_result_limit() {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .structured_predicate_handling(StructuredPredicateHandling::Error)
        .max_matches(Some(1))
        .build();
    let searcher = build_structured_searcher(&["$.latency_ms > 500"], config);
    let haystack = r#"{"events":[{"latency_ms":900},{"latency_ms":"slow"}]}"#;

    let error = searcher
        .find_structured(haystack)
        .expect_err("strict mode must scan far enough to preserve predicate errors");
    assert!(matches!(error, SearchError::StructuredPredicate { .. }));
}

#[test]
fn structured_json_try_apis_surface_parse_errors() {
    let searcher =
        build_structured_searcher(&["$.user.name"], structured_config(StructuredMode::Json));
    let error = searcher
        .try_find("{\"user\":")
        .expect_err("invalid JSON should fail");

    assert!(matches!(
        error,
        SearchError::StructuredParse {
            mode: StructuredMode::Json,
            ..
        }
    ));
    assert!(searcher.find("{\"user\":").is_empty());
    assert!(matches!(
        searcher.try_count("{\"user\":"),
        Err(SearchError::StructuredParse {
            mode: StructuredMode::Json,
            ..
        })
    ));
}

#[test]
fn structured_json_line_search_uses_global_spans() {
    let searcher = build_structured_searcher(
        &["$.items[1].status"],
        structured_config(StructuredMode::Json),
    );
    let haystack =
        "{\n  \"items\": [\n    {\"status\": \"ok\"},\n    {\"status\": \"fail\"}\n  ]\n}\n";

    let lines = searcher.find_lines(haystack);

    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].line_number, 4);
    assert_eq!(lines[0].line.trim(), "{\"status\": \"fail\"}");
    assert_eq!(lines[0].matches.len(), 1);
}

#[test]
fn structured_json_matches_nested_observability_payloads() {
    let searcher = build_structured_searcher(
        &[
            "$.records[1].resource[\"service.name\"]",
            "acme-prod",
            "$.records[1].events[1].attrs[\"db.statement\"]=SELECT 1",
        ],
        structured_config(StructuredMode::Json),
    );
    let haystack = r#"{
  "records": [
    {
      "resource": {
        "service.name": "edge-proxy",
        "deployment.environment": "staging"
      },
      "events": [
        {
          "name": "startup",
          "attrs": {
            "message": "ready"
          }
        }
      ]
    },
    {
      "resource": {
        "service.name": "payments-api",
        "deployment.environment": "prod"
      },
      "attributes": {
        "tenant.id": "acme-prod"
      },
      "events": [
        {
          "name": "http.request",
          "attrs": {
            "http.route": "/checkout",
            "http.status_code": 500
          }
        },
        {
          "name": "db.query",
          "attrs": {
            "db.system": "postgres",
            "db.statement": "SELECT 1"
          }
        }
      ]
    }
  ]
}"#;

    let mut projection = searcher
        .find_structured(haystack)
        .expect("structured search should parse JSON")
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text.to_string(),
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();
    projection.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    assert_eq!(
        projection,
        vec![
            (
                0,
                "$.records[1].resource[\"service.name\"]".to_string(),
                "\"payments-api\"".to_string(),
                StructuredMatchKind::Path,
            ),
            (
                1,
                "$.records[1].attributes[\"tenant.id\"]".to_string(),
                "\"acme-prod\"".to_string(),
                StructuredMatchKind::Value,
            ),
            (
                2,
                "$.records[1].events[1].attrs[\"db.statement\"]".to_string(),
                "\"SELECT 1\"".to_string(),
                StructuredMatchKind::PathValue,
            ),
        ]
    );
}

#[test]
fn structured_json_predicates_match_numeric_temporal_and_cross_field_filters() {
    let searcher = build_structured_searcher(
        &[
            "$.latency_ms > 500",
            "$.timestamp between \"2026-03-18T10:00:00Z\" and \"2026-03-18T10:30:00Z\"",
            "$.status=500 AND $.service=\"payments\"",
        ],
        structured_config(StructuredMode::Json),
    );
    let haystack = r#"{
  "events": [
    {
      "service": "edge",
      "status": 200,
      "latency_ms": null,
      "timestamp": "2026-03-18T09:59:00Z"
    },
    {
      "service": "payments",
      "status": 500,
      "latency_ms": 900,
      "timestamp": "2026-03-18T10:15:00Z"
    },
    {
      "service": "billing",
      "status": 500,
      "latency_ms": "slow",
      "timestamp": "not-a-timestamp"
    }
  ]
}"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured predicate search should parse JSON");
    let projection = matches
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text.to_string(),
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            // Matches come in document order, where `status` precedes the others.
            (
                2,
                "$.events[1].status".to_string(),
                "500".to_string(),
                StructuredMatchKind::Predicate,
            ),
            (
                0,
                "$.events[1].latency_ms".to_string(),
                "900".to_string(),
                StructuredMatchKind::Predicate,
            ),
            (
                1,
                "$.events[1].timestamp".to_string(),
                "\"2026-03-18T10:15:00Z\"".to_string(),
                StructuredMatchKind::Predicate,
            ),
        ]
    );
}

#[test]
fn structured_json_predicates_can_error_on_nulls_and_invalid_types() {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .structured_predicate_handling(StructuredPredicateHandling::Error)
        .build();
    let searcher = build_structured_searcher(&["$.latency_ms > 500"], config);
    let haystack = r#"{"events":[{"latency_ms":null},{"latency_ms":"slow"},{"latency_ms":900}]}"#;

    let error = searcher
        .find_structured(haystack)
        .expect_err("strict predicate handling should surface invalid data");

    assert!(matches!(
        error,
        SearchError::StructuredPredicate {
            pattern_index: 0,
            path,
            ..
        } if path == "$.events[0].latency_ms"
    ));
}

#[test]
fn structured_yaml_matches_paths_values_and_scalar_keys() {
    let searcher = build_structured_searcher(
        &[
            "Alice",
            "$.items[1][\"display name\"]",
            "$.flags.true=ok",
            "$.ports[\"8080\"]",
        ],
        structured_config(StructuredMode::Yaml),
    );
    let haystack = "user:\n  name: Alice\nitems:\n  - display name: bolt\n  - display name: nut\nflags:\n  true: ok\nports:\n  8080: web\n";

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse YAML");
    let projection = matches
        .iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path.as_str(),
                matched.node_text,
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            (0, "$.user.name", "Alice", StructuredMatchKind::Value),
            (
                1,
                "$.items[1][\"display name\"]",
                "nut",
                StructuredMatchKind::Path,
            ),
            (2, "$.flags.true", "ok", StructuredMatchKind::PathValue),
            (3, "$.ports[\"8080\"]", "web", StructuredMatchKind::Path),
        ]
    );
}

#[test]
fn structured_yaml_supports_multiple_documents() {
    let searcher = build_structured_searcher(
        &["$[1].user.name=Bob"],
        structured_config(StructuredMode::Yaml),
    );
    let haystack = "user:\n  name: Alice\n---\nuser:\n  name: Bob\n";

    let matches = searcher
        .find_structured(haystack)
        .expect("multi-document YAML should parse");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "$[1].user.name");
    assert_eq!(matches[0].node_text, "Bob");
    assert_eq!(matches[0].match_kind, StructuredMatchKind::PathValue);
}

#[test]
fn structured_yaml_paths_index_every_document_of_a_stream_on_the_scanner_too() {
    let streams = [
        "a: x\nb: 1\n---\na: y\nb: 2\n",
        "a: x\r\nb: 1\r\n---\r\na: y\r\nb: 2\r\n",
        "---\na: x\n---\na: y\n",
        "a: x\nb: 1\n---\n",
        "a: x\n---\n---\na: y\n",
        "- a: x\n---\n- a: y\n",
        "---\na: x\nb: 1\n",
        "%YAML 1.2\n---\na: x\n",
        "---\n- a: x\n- a: y\n",
    ];
    let patterns = [
        "$.a",
        "$.a=x",
        "$[0].a",
        "$[1].a",
        "$[1].a=y",
        "$[0]",
        "$[1][0].a",
    ];

    for stream in streams {
        for pattern in patterns {
            // A second pattern of another kind takes the parser instead of the scanner.
            let [alone, beside_decoy] = [vec![pattern], vec![pattern, "never"]].map(|patterns| {
                build_structured_searcher(&patterns, structured_config(StructuredMode::Yaml))
                    .find_structured(stream)
                    .expect("the streams parse")
                    .into_iter()
                    .map(|found| (found.path, found.node_text.to_string()))
                    .collect::<Vec<_>>()
            });
            assert_eq!(alone, beside_decoy, "{pattern} in {stream:?}");
        }
    }

    let two_documents = streams[0];
    let found = |pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(StructuredMode::Yaml))
            .find_structured(two_documents)
            .expect("the stream parses")
            .into_iter()
            .map(|found| found.path)
            .collect::<Vec<_>>()
    };
    assert!(found("$.a").is_empty());
    assert_eq!(found("$[1].a=y"), ["$[1].a"]);
}

#[test]
fn structured_yaml_matching_respects_ascii_case_for_scalar_values() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .structured_mode(Some(StructuredMode::Yaml))
        .build();
    let searcher = build_structured_searcher(&["alice"], config);
    let haystack = "user:\n  name: Alice\nother: ALICE\n";

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse YAML");
    let matched_paths = matches
        .iter()
        .map(|matched| matched.path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(matched_paths, vec!["$.user.name", "$.other"]);
}

#[test]
fn structured_yaml_try_apis_surface_parse_errors() {
    let searcher =
        build_structured_searcher(&["$.user.name"], structured_config(StructuredMode::Yaml));
    let error = searcher
        .try_find("user:\n  - [broken\n")
        .expect_err("invalid YAML should fail");

    assert!(matches!(
        error,
        SearchError::StructuredParse {
            mode: StructuredMode::Yaml,
            ..
        }
    ));
    assert!(searcher.find("user:\n  - [broken\n").is_empty());
}

#[test]
fn structured_yaml_line_search_uses_global_spans() {
    let searcher = build_structured_searcher(
        &["$.items[1].status"],
        structured_config(StructuredMode::Yaml),
    );
    let haystack = "items:\n  - status: ok\n  - status: fail\n";

    let lines = searcher.find_lines(haystack);

    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].line_number, 3);
    assert_eq!(lines[0].line.trim(), "- status: fail");
    assert_eq!(lines[0].matches.len(), 1);
}

#[test]
fn structured_yaml_matches_multi_document_kubernetes_manifests() {
    let searcher = build_structured_searcher(
        &[
            "$[0].metadata.annotations[\"app.kubernetes.io/name\"]",
            "registry.example.com/payments-worker:2026.03.1",
            "$[1].spec.template.spec.containers[1].env[0].valueFrom.secretKeyRef.name=payments-db",
        ],
        structured_config(StructuredMode::Yaml),
    );
    let haystack = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: payments-config
  annotations:
    app.kubernetes.io/name: payments-api
data:
  LOG_LEVEL: info
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: payments-api
spec:
  template:
    spec:
      containers:
        - name: web
          image: registry.example.com/payments-web:2026.03.1
        - name: worker
          image: registry.example.com/payments-worker:2026.03.1
          env:
            - name: DATABASE_NAME
              valueFrom:
                secretKeyRef:
                  name: payments-db
                  key: database
"#;

    let mut projection = searcher
        .find_structured(haystack)
        .expect("structured search should parse YAML")
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text.to_string(),
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();
    projection.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    assert_eq!(
        projection,
        vec![
            (
                0,
                "$[0].metadata.annotations[\"app.kubernetes.io/name\"]".to_string(),
                "payments-api".to_string(),
                StructuredMatchKind::Path,
            ),
            (
                1,
                "$[1].spec.template.spec.containers[1].image".to_string(),
                "registry.example.com/payments-worker:2026.03.1".to_string(),
                StructuredMatchKind::Value,
            ),
            (
                2,
                "$[1].spec.template.spec.containers[1].env[0].valueFrom.secretKeyRef.name"
                    .to_string(),
                "payments-db".to_string(),
                StructuredMatchKind::PathValue,
            ),
        ]
    );
}

#[test]
fn structured_yaml_predicates_match_numeric_temporal_and_cross_field_filters() {
    let searcher = build_structured_searcher(
        &[
            "$.latency_ms > 500",
            "$.timestamp between 2026-03-18T10:00:00Z and 2026-03-18T10:30:00Z",
            "$.status=500 AND $.service=payments",
        ],
        structured_config(StructuredMode::Yaml),
    );
    let haystack = r#"events:
  - service: edge
    status: 200
    latency_ms: null
    timestamp: 2026-03-18T09:59:00Z
  - service: payments
    status: 500
    latency_ms: 900
    timestamp: 2026-03-18T10:15:00Z
  - service: billing
    status: 500
    latency_ms: slow
    timestamp: invalid
"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured predicate search should parse YAML");
    let projection = matches
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text.to_string(),
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            // Matches come in document order, where `status` precedes the others.
            (
                2,
                "$.events[1].status".to_string(),
                "500".to_string(),
                StructuredMatchKind::Predicate,
            ),
            (
                0,
                "$.events[1].latency_ms".to_string(),
                "900".to_string(),
                StructuredMatchKind::Predicate,
            ),
            (
                1,
                "$.events[1].timestamp".to_string(),
                "2026-03-18T10:15:00Z".to_string(),
                StructuredMatchKind::Predicate,
            ),
        ]
    );
}

#[test]
fn structured_toml_matches_paths_values_and_quoted_keys() {
    let searcher = build_structured_searcher(
        &[
            "Alice",
            "$.items[1][\"display name\"]",
            "$.flags.enabled=true",
        ],
        structured_config(StructuredMode::Toml),
    );
    let haystack = r#"
[user]
name = "Alice"

[[items]]
"display name" = "bolt"

[[items]]
"display name" = "nut"

[flags]
enabled = true
"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse TOML");
    let projection = matches
        .iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path.as_str(),
                matched.node_text,
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            (0, "$.user.name", "\"Alice\"", StructuredMatchKind::Value),
            (
                1,
                "$.items[1][\"display name\"]",
                "\"nut\"",
                StructuredMatchKind::Path,
            ),
            (2, "$.flags.enabled", "true", StructuredMatchKind::PathValue),
        ]
    );
}

#[test]
fn structured_toml_parser_values_match_through_escapes_case_and_word_rules() {
    let document = "plain = \"catalog\"\nescaped = \"ca\\u0074alog\"\nloud = 'CATALOG'\n\
        word = \"the cat sat\"\nn = 12\nm = 120\n[t]\nname = \"cat\"\ntier = \"dog\"\n";
    let found = |pattern: &str, case_mode: CaseMode, whole_word: bool| {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Toml))
            .case_mode(case_mode)
            .whole_word(whole_word)
            .build();
        // The path pattern takes the parser instead of the scanner.
        build_structured_searcher(&[pattern, "$.t.name"], config)
            .find_structured(document)
            .expect("the TOML parses")
            .into_iter()
            .filter(|found| found.match_kind == StructuredMatchKind::Value)
            .map(|found| found.path)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        found("catalog", CaseMode::Sensitive, false),
        ["$.plain", "$.escaped"]
    );
    assert_eq!(
        found("catalog", CaseMode::AsciiInsensitive, false),
        ["$.plain", "$.escaped", "$.loud"]
    );
    assert_eq!(
        found("cat", CaseMode::Sensitive, true),
        ["$.word", "$.t.name"]
    );
    assert_eq!(found("12", CaseMode::Sensitive, false), ["$.n", "$.m"]);
    assert_eq!(found("dog", CaseMode::Sensitive, false), ["$.t.tier"]);
}

#[test]
fn structured_toml_matching_respects_ascii_case_for_scalar_values() {
    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .structured_mode(Some(StructuredMode::Toml))
        .build();
    let searcher = build_structured_searcher(&["alice"], config);
    let haystack = r#"
[user]
name = "Alice"
other = "ALICE"
"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured search should parse TOML");
    let matched_paths = matches
        .iter()
        .map(|matched| matched.path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(matched_paths, vec!["$.user.name", "$.user.other"]);
}

#[test]
fn structured_toml_try_apis_surface_parse_errors() {
    let searcher =
        build_structured_searcher(&["$.user.name"], structured_config(StructuredMode::Toml));
    let error = searcher
        .try_find("[user\nname = \"Alice\"")
        .expect_err("invalid TOML should fail");

    assert!(matches!(
        error,
        SearchError::StructuredParse {
            mode: StructuredMode::Toml,
            ..
        }
    ));
    assert!(searcher.find("[user\nname = \"Alice\"").is_empty());
}

#[test]
fn structured_toml_line_search_uses_global_spans() {
    let searcher = build_structured_searcher(
        &["$.items[1].status"],
        structured_config(StructuredMode::Toml),
    );
    let haystack = r#"
[[items]]
status = "ok"

[[items]]
status = "fail"
"#;

    let lines = searcher.find_lines(haystack);

    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].line_number, 6);
    assert_eq!(lines[0].line.trim(), "status = \"fail\"");
    assert_eq!(lines[0].matches.len(), 1);
}

#[test]
fn structured_toml_matches_project_configuration_with_arrays_of_tables() {
    let searcher = build_structured_searcher(
        &[
            "billing-worker",
            "$.tool.ruff.lint.select[2]=B",
            "$.tool.uv.index[1].url",
        ],
        structured_config(StructuredMode::Toml),
    );
    let haystack = r#"
[project]
name = "billing-worker"

[tool.ruff.lint]
select = ["E", "F", "B"]

[[tool.uv.index]]
name = "internal"
url = "https://packages.example.com/simple"

[[tool.uv.index]]
name = "backup"
url = "https://mirror.example.com/simple"
"#;

    let mut projection = searcher
        .find_structured(haystack)
        .expect("structured search should parse TOML")
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text.to_string(),
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();
    projection.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    assert_eq!(
        projection,
        vec![
            (
                0,
                "$.project.name".to_string(),
                "\"billing-worker\"".to_string(),
                StructuredMatchKind::Value,
            ),
            (
                1,
                "$.tool.ruff.lint.select[2]".to_string(),
                "\"B\"".to_string(),
                StructuredMatchKind::PathValue,
            ),
            (
                2,
                "$.tool.uv.index[1].url".to_string(),
                "\"https://mirror.example.com/simple\"".to_string(),
                StructuredMatchKind::Path,
            ),
        ]
    );
}

#[test]
fn structured_toml_predicates_match_numeric_temporal_and_cross_field_filters() {
    let searcher = build_structured_searcher(
        &[
            "$.latency_ms > 500",
            "$.timestamp between \"2026-03-18T10:00:00Z\" and \"2026-03-18T10:30:00Z\"",
            "$.status=500 AND $.service=\"payments\"",
        ],
        structured_config(StructuredMode::Toml),
    );
    let haystack = r#"
[[events]]
service = "edge"
status = 200
latency_ms = 100
timestamp = "2026-03-18T09:59:00Z"

[[events]]
service = "payments"
status = 500
latency_ms = 900
timestamp = "2026-03-18T10:15:00Z"

[[events]]
service = "billing"
status = 500
latency_ms = "slow"
timestamp = "invalid"
"#;

    let matches = searcher
        .find_structured(haystack)
        .expect("structured predicate search should parse TOML");
    let projection = matches
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text.to_string(),
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            // Matches come in document order, where `status` precedes the others.
            (
                2,
                "$.events[1].status".to_string(),
                "500".to_string(),
                StructuredMatchKind::Predicate,
            ),
            (
                0,
                "$.events[1].latency_ms".to_string(),
                "900".to_string(),
                StructuredMatchKind::Predicate,
            ),
            (
                1,
                "$.events[1].timestamp".to_string(),
                "\"2026-03-18T10:15:00Z\"".to_string(),
                StructuredMatchKind::Predicate,
            ),
        ]
    );
}

const TOML_WITH_DATETIMES: &str = "\
released = 2024-05-01
at = 1979-05-27T07:32:00Z
spaced = 1979-05-27 07:32:00.5-07:00
local = 1979-05-27T07:32:00
clock = 07:32:00 # trailing comment
short = 1979-05-27T07:32Z
dates = [2024-05-01, 2024-06-01]

[[events]]
when = 2026-03-18T10:15:00Z
meta = { seen = 2026-03-18 }

[[events]]
when = 2026-03-18T11:45:00Z
";

/// Searches `document` for `pattern` alone and again beside a pattern of another kind, which
/// takes the parser instead of the byte scanner, and returns the matches both agree on.
fn toml_paths_and_text(pattern: &str, document: &str) -> Vec<(String, String)> {
    let decoy = if pattern.starts_with('$') {
        "never-present"
    } else {
        "$.never_present"
    };
    let [alone, beside_decoy] = [vec![pattern], vec![pattern, decoy]].map(|patterns| {
        build_structured_searcher(&patterns, structured_config(StructuredMode::Toml))
            .find_structured(document)
            .expect("the TOML parses")
            .into_iter()
            .map(|found| (found.path, found.node_text.to_string()))
            .collect::<Vec<_>>()
    });
    assert_eq!(alone, beside_decoy, "{pattern}");
    alone
}

#[test]
fn structured_toml_finds_datetimes_as_written() {
    let found = |pattern: &str| toml_paths_and_text(pattern, TOML_WITH_DATETIMES);
    let pairs = |expected: &[(&str, &str)]| {
        expected
            .iter()
            .map(|&(path, text)| (path.to_owned(), text.to_owned()))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        found("1979"),
        pairs(&[
            ("$.at", "1979-05-27T07:32:00Z"),
            ("$.spaced", "1979-05-27 07:32:00.5-07:00"),
            ("$.local", "1979-05-27T07:32:00"),
            ("$.short", "1979-05-27T07:32Z"),
        ])
    );
    assert_eq!(found("$.clock"), pairs(&[("$.clock", "07:32:00")]));
    assert_eq!(found("$.dates[1]"), pairs(&[("$.dates[1]", "2024-06-01")]));
    assert_eq!(
        found("$.events[0].meta.seen"),
        pairs(&[("$.events[0].meta.seen", "2026-03-18")])
    );
    assert_eq!(
        found("$.released=2024-05-01"),
        pairs(&[("$.released", "2024-05-01")])
    );
}

#[test]
fn structured_toml_datetimes_compare_as_timestamps_and_dates_as_text() {
    let found = |pattern: &str| {
        toml_paths_and_text(pattern, TOML_WITH_DATETIMES)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>()
    };

    assert_eq!(found("$.when > 2026-03-18T11:00:00Z"), ["$.events[1].when"]);
    assert_eq!(
        found("$.when BETWEEN 2026-03-18T10:00:00Z AND 2026-03-18T10:30:00Z"),
        ["$.events[0].when"]
    );
    // 07:32 at UTC-7 is 14:32 UTC.
    assert_eq!(found("$.spaced > 1979-05-27T14:00:00Z"), ["$.spaced"]);
    assert_eq!(found("$.released == 2024-05-01"), ["$.released"]);
    assert_eq!(found("$.seen == 2026-03-18"), ["$.events[0].meta.seen"]);
    // A local datetime names no instant, so it never satisfies a timestamp comparison.
    assert!(found("$.local < 2000-01-01T00:00:00Z").is_empty());
}

#[test]
fn structured_toml_strict_predicates_reject_local_datetimes_against_timestamps() {
    let strict = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Toml))
        .structured_predicate_handling(StructuredPredicateHandling::Error)
        .build();
    let searcher = build_structured_searcher(&["$.local < 2000-01-01T00:00:00Z"], strict);

    let error = searcher
        .find_structured(TOML_WITH_DATETIMES)
        .expect_err("a local datetime is not a timestamp");

    assert!(
        error.to_string().contains("RFC 3339 timestamp"),
        "unexpected error: {error}"
    );
}

#[test]
fn structured_toml_tables_span_every_line_that_defines_them() {
    let document = "\
a.b.c = \"x\"
a.d = 1

[x.y]
k = 1

[[jobs]]
name = \"one\"

[[jobs]]
name = \"two\"
";
    let found = |pattern: &str| {
        toml_paths_and_text(pattern, document)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
    };

    assert_eq!(found("$.a"), ["a.b.c = \"x\"\na.d = 1"]);
    assert_eq!(found("$.a.b"), ["a.b.c = \"x\""]);
    assert_eq!(found("$.x"), ["[x.y]\nk = 1"]);
    assert_eq!(
        found("$.jobs"),
        ["[[jobs]]\nname = \"one\"\n\n[[jobs]]\nname = \"two\""]
    );
}

/// A document with `alpha` first, `omega` last, and an `events` list between them long
/// enough to be scanned in parallel chunks.
fn large_predicate_document(format: StructuredMode) -> String {
    let events = 12_000;
    let service = |index: usize| ["edge", "payments", "billing"][index % 3];
    match format {
        StructuredMode::Json => {
            let items = (0..events)
                .map(|index| {
                    format!(
                        r#"{{"id":{index},"service":"{}","status":{},"latency_ms":{}}}"#,
                        service(index),
                        if index % 7 == 0 { 500 } else { 200 },
                        index % 1_000
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"{{"alpha":1,"events":[{items}],"omega":2}}"#)
        }
        StructuredMode::Yaml => {
            let items = (0..events)
                .map(|index| {
                    format!(
                        "  - id: {index}\n    service: {}\n    status: {}\n    latency_ms: {}\n",
                        service(index),
                        if index % 7 == 0 { 500 } else { 200 },
                        index % 1_000
                    )
                })
                .collect::<String>();
            format!("alpha: 1\nevents:\n{items}omega: 2\n")
        }
        StructuredMode::Toml => {
            let items = (0..events)
                .map(|index| {
                    format!(
                        "[[events]]\nid = {index}\nservice = \"{}\"\nstatus = {}\nlatency_ms = {}\n",
                        service(index),
                        if index % 7 == 0 { 500 } else { 200 },
                        index % 1_000
                    )
                })
                .collect::<String>();
            let filler = (0..events)
                .map(|index| format!("key_{index} = {index}\n"))
                .collect::<String>();
            format!("alpha = 1\n{filler}omega = 2\n{items}")
        }
    }
}

#[test]
fn parallel_predicate_search_matches_single_thread_search() {
    let patterns = [
        "$.latency_ms > 900",
        "$.status=500 AND $.service=\"payments\"",
        "$.alpha=1 AND $.omega=2",
    ];
    let single_thread = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("single-thread pool should build");

    for format in [
        StructuredMode::Json,
        StructuredMode::Yaml,
        StructuredMode::Toml,
    ] {
        let searcher = build_structured_searcher(&patterns, structured_config(format));
        let haystack = large_predicate_document(format);
        let project = |matches: Vec<byteseek::StructuredMatch<'_>>| {
            matches
                .into_iter()
                .map(|matched| {
                    (
                        matched.pattern_index,
                        matched.start,
                        matched.end,
                        matched.path,
                    )
                })
                .collect::<Vec<_>>()
        };

        let parallel = project(
            searcher
                .find_structured(&haystack)
                .expect("supported document"),
        );
        let sequential = single_thread.install(|| {
            project(
                searcher
                    .find_structured(&haystack)
                    .expect("supported document"),
            )
        });

        assert!(
            parallel.iter().any(|(pattern, ..)| *pattern == 2),
            "{format:?}"
        );
        assert_eq!(parallel, sequential, "{format:?}");
    }
}

#[test]
fn truncated_json_literals_are_reported_or_matched_without_panicking() {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .build();

    for document in [
        r#"{"a":t}"#,
        r#"{"a":f}"#,
        r#"{"a":n}"#,
        r#"{"b":1,"a":tr}"#,
        "[t]",
        r#"[{"a":fals}]"#,
        // Other text in a literal's place, with a character straddling the literal's length.
        "{\"a\":txy\u{e9}}",
        "{\"a\":fxyz\u{1f600}}",
        "[nu\u{20ac}]",
    ] {
        for pattern in ["$.a", "$.a=true", "$[0]", "$[0].a", "tru", "$.a > 1"] {
            let searcher = Searcher::new([pattern], config).expect("pattern should compile");

            // Malformed input may fail to parse or match leniently; either way it returns.
            let _ = searcher.find_structured(document);
            let _ = searcher.try_count(document);
        }
    }
}

/// Returns each match's path and text for one pattern over one document.
fn structured_hits(mode: StructuredMode, pattern: &str, document: &str) -> Vec<(String, String)> {
    let config = SearchConfig::builder().structured_mode(Some(mode)).build();
    Searcher::new([pattern], config)
        .expect("pattern should compile")
        .find_structured(document)
        .expect("document should parse")
        .into_iter()
        .map(|matched| (matched.path, matched.node_text.to_owned()))
        .collect()
}

#[test]
fn yaml_trailing_comments_are_not_part_of_values() {
    let document =
        "status: failed # retried twice\nurl: http://host/#anchor\nitems:\n  - failed # again\n";
    let failed = |path: &str| (path.to_owned(), "failed".to_owned());

    assert_eq!(
        structured_hits(StructuredMode::Yaml, "failed", document),
        [failed("$.status"), failed("$.items[0]")]
    );
    assert_eq!(
        structured_hits(StructuredMode::Yaml, "$.status=failed", document),
        [failed("$.status")]
    );
    assert_eq!(
        structured_hits(
            StructuredMode::Yaml,
            "$.status=\"failed\" AND $.url=\"http://host/#anchor\"",
            document
        )
        .len(),
        1
    );
    // A `#` inside a word is not a comment.
    assert_eq!(
        structured_hits(StructuredMode::Yaml, "anchor", document),
        [("$.url".to_owned(), "http://host/#anchor".to_owned())]
    );
    // Nor is one inside quotes, however far the value sits from its key or dash.
    let spaced = "msg:  \"a # b\"\nitems:\n  -  \"x # b\"\n";
    assert_eq!(
        structured_hits(StructuredMode::Yaml, "b", spaced),
        [
            ("$.msg".to_owned(), "\"a # b\"".to_owned()),
            ("$.items[0]".to_owned(), "\"x # b\"".to_owned())
        ]
    );
}

#[test]
fn yaml_values_outside_the_byte_scanner_keep_parser_paths() {
    let paths = |pattern: &str, document: &str| {
        structured_hits(StructuredMode::Yaml, pattern, document)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>()
    };

    // A block scalar's lines are text, not mapping entries.
    assert_eq!(
        paths("needle", "note: |\n  key: needle\nlast: 1\n"),
        ["$.note"]
    );
    assert_eq!(paths("needle", "note: >\n  key: needle\n"), ["$.note"]);
    // Flow collections have elements, and entries after an alias keep their own keys.
    assert_eq!(paths("needle", "tags: [first, needle]\n"), ["$.tags[1]"]);
    assert_eq!(paths("needle", "map: {key: needle}\n"), ["$.map.key"]);
    assert_eq!(
        paths("needle", "items:\n  - [first, needle]\n"),
        ["$.items[0][1]"]
    );
    assert_eq!(
        paths(
            "needle",
            "base: &shared needle\nother: *shared\nlast: needle\n"
        ),
        ["$.base", "$.last"]
    );
}

#[test]
fn toml_path_navigation_matches_keys_only_at_line_starts() {
    let document =
        "[[events]]\nusername = \"wrong\"\nnote = \"name = wrong\"\n  name = \"right\"\n";

    let hits = structured_hits(StructuredMode::Toml, "$.events[0].name", document);

    assert_eq!(hits.len(), 1);
    assert!(hits[0].1.contains("right"), "{hits:?}");
}

#[test]
fn toml_path_navigation_ends_an_entry_at_a_header_on_its_first_line() {
    // The first entry is empty: the `id` after the second header is not its own.
    let document = "[[events]]\n[[events]]\nid = 5\n[meta]\nid = 9\n";

    let first = structured_hits(StructuredMode::Toml, "$.events[0].id", document);
    let second = structured_hits(StructuredMode::Toml, "$.events[1].id", document);

    assert!(first.is_empty(), "{first:?}");
    assert_eq!(second.len(), 1);
    assert!(second[0].1.contains('5'), "{second:?}");
}

#[test]
fn yaml_predicates_skipping_items_match_the_parser() {
    // Items without the literal, with it under another key, with it but failing the other
    // clause, holding an alias to it, and a matching object outside every item.
    let mut document = String::from(
        "service:\n  name: svc-7\npayload:\n  status: failed\nshared: &name svc-7\nevents:\n",
    );
    for index in 0..400 {
        let name = if index % 9 == 0 { "svc-7" } else { "svc-12" };
        let status = if index % 2 == 0 { "failed" } else { "ok" };
        let note = if index % 5 == 0 { "svc-7" } else { "none" };
        document.push_str(&format!(
            "  - id: {index}\n    service:\n      name: {name}\n    payload:\n      status: {status}\n      \
             note: {note}\n    tags:\n      - svc-7\n      - other\n"
        ));
    }
    let with_alias =
        format!("{document}  - service:\n      name: *name\n    payload:\n      status: failed\n");
    let pattern = "$.service.name=\"svc-7\" AND $.payload.status=failed";

    for document in [document.as_str(), with_alias.as_str()] {
        let spans = |handling: StructuredPredicateHandling| {
            let config = SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .structured_predicate_handling(handling)
                .build();
            Searcher::new([pattern], config)
                .expect("pattern should compile")
                .find_structured(document)
                .expect("document should parse")
                .into_iter()
                .map(|matched| (matched.start, matched.end, matched.path))
                .collect::<Vec<_>>()
        };

        let scanned = spans(StructuredPredicateHandling::Ignore);

        // Strict handling always takes the parser.
        assert_eq!(scanned, spans(StructuredPredicateHandling::Error));
        assert!(scanned.len() > 400 / 18, "{}", scanned.len());
    }
}

#[test]
fn unterminated_quoted_path_keys_are_not_paths() {
    let json = "{\"key\": 1, \"a\": {\"b\": 2}}";

    for pattern in ["$[\"", "$[\"\\", "$[\"key", "$.a[\"b\\"] {
        // Not a path, so the pattern is plain text, which this document does not hold.
        let searcher =
            build_structured_searcher(&[pattern], structured_config(StructuredMode::Json));
        assert_eq!(searcher.try_find(json), Ok(Vec::new()), "{pattern}");
    }
}

#[test]
fn toml_values_with_a_lone_quote_keep_their_span() {
    let searcher = build_structured_searcher(&["abc"], structured_config(StructuredMode::Toml));

    // The value never closes, so all of it up to the line end is the value.
    let unclosed = "k = \"abc\u{e9}";
    let found = searcher.find(unclosed);
    assert_eq!(found.len(), 1);
    assert_eq!(&unclosed[found[0].start..found[0].end], "\"abc\u{e9}");

    // A lone quote holds no text.
    for haystack in ["a = \"", "a = '", "[t]\na = '\n"] {
        assert!(searcher.find(haystack).is_empty(), "{haystack:?}");
    }
}

#[test]
fn toml_paths_into_plain_tables_are_found() {
    let haystack = "[user]\nname = \"Alice\"\n\n[[items]]\nstatus = \"ok\"\n";
    let searcher = build_structured_searcher(
        &[
            "$.user.name",
            "$.items[0].status",
            "$.user.name=Alice",
            "$.user.age",
        ],
        structured_config(StructuredMode::Toml),
    );

    let found = searcher
        .find_structured(haystack)
        .expect("valid TOML")
        .into_iter()
        .map(|matched| (matched.pattern_index, matched.node_text))
        .collect::<Vec<_>>();

    assert!(found.contains(&(0, "\"Alice\"")), "{found:?}");
    assert!(found.contains(&(1, "\"ok\"")), "{found:?}");
    assert!(found.iter().all(|&(pattern, _)| pattern != 3), "{found:?}");
}

#[test]
fn yaml_aliases_to_their_own_ancestors_are_parse_errors() {
    let cyclic = [
        "&a [*a]\n",
        "a: &a\n  b: *a\n",
        "&a\nk: [1, {x: *a}]\n",
        "---\nb: 1\n---\n&a [*a]\n",
    ];
    // A path with plain text is searched in the parsed document, where aliases are followed.
    let searcher =
        build_structured_searcher(&["$.a", "x"], structured_config(StructuredMode::Yaml));

    for haystack in cyclic {
        assert!(
            matches!(
                searcher.try_find(haystack),
                Err(SearchError::StructuredParse {
                    mode: StructuredMode::Yaml,
                    ..
                })
            ),
            "{haystack:?}"
        );
        assert!(searcher.find(haystack).is_empty(), "{haystack:?}");
    }
    // An anchor used twice is shared, not cyclic.
    assert!(searcher.try_find("a: &x [1, 2]\nb: *x\nc: *x\n").is_ok());
}

#[test]
fn yaml_aliases_that_multiply_values_are_parse_errors() {
    let mut laughs = String::from("l0: &l0 [x, x, x, x, x, x, x, x, x, x]\n");
    for level in 1..9 {
        let previous = format!("*l{}", level - 1);
        let items = [previous.as_str(); 10].join(", ");
        laughs.push_str(&format!("l{level}: &l{level} [{items}]\n"));
    }
    let searcher =
        build_structured_searcher(&["$.l8", "x"], structured_config(StructuredMode::Yaml));

    let outcome = searcher.try_find(&laughs);

    assert!(matches!(
        outcome,
        Err(SearchError::StructuredParse {
            mode: StructuredMode::Yaml,
            ..
        })
    ));
}

#[test]
fn a_byte_order_mark_is_skipped_and_offsets_count_it() {
    let cases = [
        (StructuredMode::Yaml, "k: needle\n"),
        (StructuredMode::Json, "{\"k\": \"needle\"}"),
        (StructuredMode::Toml, "k = \"needle\"\n"),
    ];
    for (mode, document) in cases {
        let marked = format!("\u{feff}{document}");
        for patterns in [&["$.k", "needle"][..], &["$.k"], &["needle"]] {
            let searcher = build_structured_searcher(patterns, structured_config(mode));
            let plain = searcher.find_structured(document).expect("valid document");

            let with_mark = searcher.find_structured(&marked).expect("valid document");

            assert!(!plain.is_empty(), "{mode:?} {patterns:?}");
            assert_eq!(with_mark.len(), plain.len(), "{mode:?} {patterns:?}");
            for (shifted, original) in with_mark.iter().zip(&plain) {
                assert_eq!(shifted.start, original.start + 3);
                assert_eq!(shifted.end, original.end + 3);
                assert_eq!(&marked[shifted.start..shifted.end], original.node_text);
            }
        }
    }
}

#[test]
fn json_path_values_compare_unescaped_text_beyond_ascii() {
    let json = r#"{"name": "Zoë \"Z\" 🙂", "pair": "🙂 ok", "raw": "Zoë \"q\""}"#;
    let searcher = build_structured_searcher(
        &[
            "$.name=Zoë \"Z\" 🙂",
            "$.pair=🙂 ok",
            "$.raw=Zoë \"q\"",
            "$.raw=Zoe",
        ],
        structured_config(StructuredMode::Json),
    );

    let matched = searcher
        .find_structured(json)
        .expect("valid JSON")
        .into_iter()
        .map(|matched| matched.pattern_index)
        .collect::<Vec<_>>();

    assert_eq!(matched, [0, 1, 2]);
}

#[test]
fn toml_nested_too_deeply_is_a_parse_error_however_it_nests() {
    const LEVELS: usize = 100_000;
    let searcher =
        build_structured_searcher(&["$.a", "x"], structured_config(StructuredMode::Toml));
    let brackets = format!("a = {}{}\n", "[".repeat(LEVELS), "]".repeat(LEVELS));
    let dotted_key = format!("{}a = 1\n", "a.".repeat(LEVELS));
    let header = format!("[{}a]\nb = 1\n", "a.".repeat(LEVELS));
    // A closing run of four quotes ends a multi-line string that ends in a quote.
    let after_quotes = format!(
        "a = [\"\"\"x\"\"\"\", {}{}]\n",
        "[".repeat(LEVELS),
        "]".repeat(LEVELS)
    );

    for document in [brackets, dotted_key, header, after_quotes] {
        assert!(matches!(
            searcher.try_find(&document),
            Err(SearchError::StructuredParse {
                mode: StructuredMode::Toml,
                ..
            })
        ));
    }
}

#[test]
fn parsed_documents_answer_paths_with_the_named_node_only() {
    // Plain text beside a path sends the whole pattern set to the parsed document.
    let paths_of = |mode, patterns: &[&str], document: &str| {
        build_structured_searcher(patterns, structured_config(mode))
            .find_structured(document)
            .expect("valid document")
            .into_iter()
            .map(|matched| matched.path)
            .collect::<Vec<_>>()
    };
    let json = r#"{"user": {"name": "A"}, "username": "x", "a": 10}"#;

    assert_eq!(
        paths_of(StructuredMode::Json, &["$.user", "zzz"], json),
        ["$.user"]
    );
    assert_eq!(
        paths_of(StructuredMode::Json, &["$.a=1", "zzz"], json),
        [""; 0]
    );
    assert_eq!(
        paths_of(StructuredMode::Json, &["$.a=10", "zzz"], json),
        ["$.a"]
    );
    assert_eq!(
        paths_of(StructuredMode::Toml, &["$.a"], "ab = 1\n"),
        [""; 0]
    );
    assert_eq!(
        paths_of(StructuredMode::Toml, &["$.a=1"], "a = 10\n"),
        [""; 0]
    );
    assert_eq!(
        paths_of(StructuredMode::Toml, &["$.a"], "a = 10\n"),
        ["$.a"]
    );
    assert_eq!(
        paths_of(StructuredMode::Yaml, &["$[0].a"], "---\n- ab: 1\n"),
        [""; 0]
    );
}

#[test]
fn toml_paths_are_answered_from_the_entry_they_name() {
    let found = |document: &str, pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(StructuredMode::Toml))
            .find_structured(document)
            .expect("valid TOML")
            .into_iter()
            .map(|matched| matched.node_text.to_owned())
            .collect::<Vec<_>>()
    };

    // A key of the table after the entry is no key of the entry.
    let later_table = "[[e]]\na = 1\n[t]\nb = 2\n";
    assert_eq!(found(later_table, "$.e[0].b"), [""; 0]);
    assert_eq!(found(later_table, "$.e[0].a"), ["1"]);
    // Sub-tables, tight or spaced spellings and inline arrays hold the entry's values too.
    assert_eq!(found("[[e]]\n[e.p]\ns = 1\n", "$.e[0].p.s"), ["1"]);
    assert_eq!(found("[[e]]\na=1\n", "$.e[0].a"), ["1"]);
    assert_eq!(found("[[e]]\na = 1\n[[ e ]]\na = 2\n", "$.e[1].a"), ["2"]);
    assert_eq!(found("e = [{ a = \"ok\" }]\n", "$.e[0].a"), ["\"ok\""]);
    assert_eq!(found("[[e]]\nt = [1, 2]\n", "$.e[0].t[1]"), ["2"]);
    // A header may end with spaces, be indented, or hold a dot that a quoted key holds too.
    let spaced = "[[e]]\na = 1\n[[e ]]\na = 2\n[[e]]\na = 3\n";
    assert_eq!(found(spaced, "$.e[1].a"), ["2"]);
    let indented_table = "[[e]]\na = 1\n  [t]\n  b = 2\n";
    assert_eq!(found(indented_table, "$.e[0].b"), [""; 0]);
    let indented_entry = "  [[e]]\n  a = \"x\"\n[[e]]\na = \"y\"\n";
    assert_eq!(found(indented_entry, "$.e[0].a"), ["\"x\""]);
    assert_eq!(found("[[a.b]]\nx = 1\n", "$[\"a.b\"][0].x"), [""; 0]);
    assert_eq!(
        found("[[e]]\n\"p.s\" = 1\n[e.p]\ns = 2\n", "$.e[0].p.s"),
        ["2"]
    );
    // Lines of a string that spans lines are no keys of the entry.
    let long_string = "[[e]]\nd = \"\"\"\na = \"fake\"\n\"\"\"\na = \"real\"\n";
    assert_eq!(found(long_string, "$.e[0].a"), ["\"real\""]);
}

#[test]
fn single_equality_predicates_match_at_any_depth_in_every_format() {
    let paths = |mode, pattern: &str, document: &str| -> Vec<String> {
        build_structured_searcher(&[pattern], structured_config(mode))
            .find_structured(document)
            .expect("valid document")
            .into_iter()
            .map(|found| found.path)
            .collect()
    };
    let json = r#"{"events":[{"vip":true,"tier":"gold","retry":null},
        {"vip":false,"tier":"free","retry":2}]}"#;
    let yaml = "events:\n  - vip: true\n    tier: gold\n  - vip: false\n    tier: free\n";
    let yaml_flow = "events:\n  - {vip: true, tier: gold}\n  - {vip: false, tier: free}\n";
    let toml =
        "[[events]]\nvip = true\ntier = \"gold\"\n\n[[events]]\nvip = false\ntier = \"free\"\n";
    let toml_inline = "events = [{vip = true, tier = \"gold\"}, {vip = false, tier = \"free\"}]\n";

    for (mode, document) in [
        (StructuredMode::Json, json),
        (StructuredMode::Yaml, yaml),
        (StructuredMode::Yaml, yaml_flow),
        (StructuredMode::Toml, toml),
        (StructuredMode::Toml, toml_inline),
    ] {
        for (pattern, expected) in [
            ("$.vip == true", "$.events[0].vip"),
            ("$.vip = true", "$.events[0].vip"),
            ("$.tier = \"gold\"", "$.events[0].tier"),
        ] {
            assert_eq!(
                paths(mode, pattern, document),
                [expected],
                "{mode:?} {pattern}"
            );
        }
    }
    assert_eq!(
        paths(StructuredMode::Json, "$.retry == null", json),
        ["$.events[0].retry"]
    );
    // Without spaces, `=` keeps the exact path and the value as written.
    assert!(paths(StructuredMode::Json, "$.vip=true", json).is_empty());
    assert_eq!(
        paths(StructuredMode::Json, "$.events[0].vip=true", json),
        ["$.events[0].vip"]
    );
}

#[test]
fn path_values_compare_what_a_scalar_says_not_how_it_is_written() {
    let count = |mode, document: &str, pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(mode))
            .find_structured(document)
            .expect("valid document")
            .len()
    };
    let yaml = |document: &str, pattern: &str| count(StructuredMode::Yaml, document, pattern);
    let toml = |document: &str, pattern: &str| count(StructuredMode::Toml, document, pattern);

    assert_eq!(yaml("status: \"failed\"\n", "$.status=failed"), 1);
    assert_eq!(yaml("status: \"failed\"\n", "$.status=\"failed\""), 0);
    assert_eq!(yaml("status: 'it''s'\n", "$.status=it's"), 1);
    assert_eq!(yaml("status: \"a\\tb\"\n", "$.status=a\tb"), 1);
    assert_eq!(yaml("a: ~\n", "$.a=null"), 1);
    assert_eq!(yaml("a: True\n", "$.a=true"), 1);
    assert_eq!(yaml("a: True\n", "$.a=True"), 0);
    assert_eq!(yaml("a: plain words\n", "$.a=plain words"), 1);
    assert_eq!(toml("[[e]]\na = \"x\\\"y\"\n", "$.e[0].a=x\"y"), 1);
    assert_eq!(toml("[[e]]\na = \"\"\"x\"\"\"\n", "$.e[0].a=x"), 1);
    assert_eq!(toml("[[e]]\na = 'x'\n", "$.e[0].a=x"), 1);
    assert_eq!(toml("[[e]]\na = 12\n", "$.e[0].a=12"), 1);
    // An array holds no text of its own, and a plain scalar can go on in the next line.
    assert_eq!(toml("[[e]]\na = [1, 2]\n", "$.e[0].a=[1, 2]"), 0);
    assert_eq!(yaml("a: two\n  lines\n", "$.a=two"), 0);
    assert_eq!(yaml("a: two\n  lines\n", "$.a=two lines"), 1);
    assert_eq!(yaml("a: two\n\n  # note\nb: 1\n", "$.a=two"), 1);
    assert_eq!(yaml("e:\n  - a: two\n    b: 1\n", "$.e[0].a=two"), 1);
    assert_eq!(yaml("e:\n  - a: two\n      lines\n", "$.e[0].a=two"), 0);
}

#[test]
fn paths_to_values_that_span_lines_report_the_whole_value() {
    let text = |mode, document: &str, pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(mode))
            .find_structured(document)
            .expect("valid document")
            .into_iter()
            .map(|matched| matched.node_text.to_owned())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        text(StructuredMode::Toml, "[[e]]\na = [\n  1,\n]\n", "$.e[0].a"),
        ["[\n  1,\n]"]
    );
    assert_eq!(
        text(StructuredMode::Toml, "[[e]]\na = [\"x#y\"]\n", "$.e[0].a"),
        ["[\"x#y\"]"]
    );
    assert_eq!(
        text(
            StructuredMode::Yaml,
            "e:\n  - two\n    lines\n  - x\n",
            "$.e[0]"
        ),
        ["two\n    lines"]
    );
    assert_eq!(
        text(StructuredMode::Yaml, "e:\n  - two\n  - x\n", "$.e[0]"),
        ["two"]
    );
    assert_eq!(
        text(StructuredMode::Yaml, "a: 'it''s'\n", "$.a"),
        ["'it''s'"]
    );
    // Value searches read the lines of a scalar as one scalar too.
    let found = |pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(StructuredMode::Yaml))
            .find_structured("a: two\n  lines\nb: x\n")
            .expect("valid YAML")
            .into_iter()
            .map(|matched| (matched.path, matched.node_text.to_owned()))
            .collect::<Vec<_>>()
    };
    let whole = [("$.a".to_owned(), "two\n  lines".to_owned())];
    assert_eq!(found("lines"), whole);
    assert_eq!(found("two"), whole);
    assert_eq!(found("$.a=two"), []);
}

#[test]
fn toml_headers_without_a_closing_bracket_end_with_their_line() {
    let document = format!("[t]\n{}k = \"needle\"\n", "[x\n".repeat(50_000));
    let searcher = build_structured_searcher(&["needle"], structured_config(StructuredMode::Toml));

    // Not valid TOML: what matters is that the search ends, each header read once.
    assert!(searcher.find(&document).len() <= 1);
}

#[test]
fn several_value_patterns_report_each_pattern_a_value_matches() {
    let searcher =
        build_structured_searcher(&["foo", "bar"], structured_config(StructuredMode::Json));
    let indexes = |document: &str| {
        searcher
            .find_structured(document)
            .expect("valid JSON")
            .into_iter()
            .map(|matched| matched.pattern_index)
            .collect::<Vec<_>>()
    };

    assert_eq!(indexes(r#"{"a": "bar"}"#), [1]);
    assert_eq!(indexes(r#"{"a": "foobar"}"#), [0, 1]);
}

#[test]
fn a_path_the_scanners_cannot_read_does_not_hide_the_other_patterns() {
    let searcher = build_structured_searcher(
        &["$[\"a=b\"]=1", "$.c=2"],
        structured_config(StructuredMode::Json),
    );

    let found = searcher
        .find_structured(r#"{"a=b": 1, "c": 2}"#)
        .expect("valid JSON")
        .into_iter()
        .map(|matched| matched.pattern_index)
        .collect::<Vec<_>>();

    assert!(found.contains(&1), "{found:?}");
}

#[test]
fn yaml_aliases_that_repeat_long_text_are_parse_errors() {
    let long = "A".repeat(64 * 1024);
    let aliases = vec!["*k"; 20_000].join(", ");
    // The alias count is modest; it's the expanded text size that must be rejected.
    let repeated_value = format!("k: &k \"{long}\"\nl: [{aliases}]\n");
    let mut repeated_key = format!("k: &k \"{long}\"\nd: ");
    repeated_key.push_str(&"{*k : ".repeat(400));
    repeated_key.push('x');
    repeated_key.push_str(&"}".repeat(400));
    repeated_key.push('\n');
    let searcher =
        build_structured_searcher(&["$", "zzz"], structured_config(StructuredMode::Yaml));

    for document in [repeated_value, repeated_key] {
        assert!(matches!(
            searcher.try_find(&document),
            Err(SearchError::StructuredParse {
                mode: StructuredMode::Yaml,
                ..
            })
        ));
    }
    // Large text without alias expansion is fine.
    let plain = format!("k: \"{long}\"\nl: [{}]\n", vec!["1"; 20_000].join(", "));
    assert!(searcher.try_find(&plain).is_ok());
}

#[test]
fn toml_arrays_of_tables_are_numbered_per_name_however_many_names_there_are() {
    let mut document = String::new();
    for index in 0..60_000 {
        document.push_str(&format!("[[t{index}]]\nk = \"v{index}\"\n"));
    }
    document.push_str("[[t7]]\nk = \"again\"\n");
    let paths = |pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(StructuredMode::Toml))
            .find_structured(&document)
            .expect("valid TOML")
            .into_iter()
            .map(|matched| matched.path)
            .collect::<Vec<_>>()
    };

    assert_eq!(paths("v59999"), ["$.t59999[0].k"]);
    assert_eq!(paths("again"), ["$.t7[1].k"]);
}

/// `(path, start, end)` of each match of `pattern`, alone and then beside a value pattern that
/// matches nothing, which sends the search to the full parser.
fn scanned_and_parsed(
    mode: StructuredMode,
    document: &str,
    pattern: &str,
) -> [Vec<(String, usize, usize)>; 2] {
    [&[pattern][..], &[pattern, "zzzz-no-such-value"]].map(|patterns| {
        let mut found = build_structured_searcher(patterns, structured_config(mode))
            .find_structured(document)
            .expect("valid document")
            .into_iter()
            .map(|matched| (matched.path, matched.start, matched.end))
            .collect::<Vec<_>>();
        found.sort();
        found
    })
}

#[test]
fn toml_scans_agree_with_the_parser_on_syntax_beyond_plain_key_lines() {
    let cases: [(&str, &[&str]); 7] = [
        (
            "[[events]]\nuser = {vip = true, n = 5}\n[[events]]\nuser = {vip = false, n = 7}\n",
            &[
                "$.vip == true",
                "$.n > 6",
                "true",
                "5",
                "$.events[0].user.n",
            ],
        ),
        (
            "[[events]]\nid = 1\n[events.user]\nvip = true\n[[events]]\nid = 2\n[events.user]\nvip = false\n",
            &["$.vip == false", "false", "$.events[1].user.vip", "2"],
        ),
        (
            "[[a]]\nx = 1\n[[a]]\nx = 2\n[[a.b]]\ny = 3\n[[a.b]]\ny = 4\n",
            &["4", "$.y > 3", "$.a[1].b[1].y"],
        ),
        (
            "ports = [80, 443]\nmulti = [\n  1,\n  2,\n]\nname = \"x\"\n",
            &["443", "2", "x", "$.multi[1] == 2", "$.ports[1]"],
        ),
        ("\"a.b\" = 1\nc = 2\n", &["1", "2", "$.c == 2"]),
        (
            "text = \"\"\"\nkey = 5\nmore\"\"\"\nkey = 6\n",
            &["5", "6", "$.key == 6"],
        ),
        (
            "a = 1\nn = 5\n[t]\nn = 6\n",
            &["5", "6", "$.n == 6", "$.t.n"],
        ),
    ];
    for (document, patterns) in cases {
        for pattern in patterns {
            let [scanned, parsed] = scanned_and_parsed(StructuredMode::Toml, document, pattern);
            assert!(!parsed.is_empty(), "{pattern} in {document:?}");
            assert_eq!(scanned, parsed, "{pattern} in {document:?}");
        }
    }
}

#[test]
fn toml_sub_tables_of_arrays_of_tables_keep_their_element_index() {
    let mut document = String::new();
    for index in 0..60_000 {
        let vip = index % 7 == 0;
        document.push_str(&format!(
            "[[events]]\nid = {index}\n[events.user]\nvip = {vip}\n"
        ));
    }

    let [scanned, parsed] =
        scanned_and_parsed(StructuredMode::Toml, &document, "$.user.vip == true");

    assert_eq!(scanned.len(), 60_000_usize.div_ceil(7));
    assert_eq!(scanned[0].0, "$.events[0].user.vip");
    assert_eq!(scanned, parsed);
}

#[test]
fn yaml_scans_agree_with_the_parser_on_flow_items_and_sequence_bounds() {
    let cases: [(&str, &[&str]); 6] = [
        (
            "events:\n  - {vip: true, n: 5}\n  - {vip: false, n: 7}\n",
            &[
                "$.vip == true",
                "$.n > 6",
                "true",
                "e",
                "vip",
                "$.events[0].n",
                "$.events[1]",
            ],
        ),
        (
            "events:\n  - n: 5\n    vip: true\n",
            &["$.n", "$.n=5", "$.events[0].n", "$.events[0].n=5"],
        ),
        ("a:\n  - 1\nb:\n  - 2\n", &["$.a[0]", "$.a[1]", "$.b[0]"]),
        (
            "a:\n- 1\n- 2\nb:\n- 3\n",
            &["$.a[1]", "$.a[2]", "$.b[0]", "$[0]"],
        ),
        (
            "- x\n- n: 5\n  m: 6\n",
            &["$[0].n", "$[1].n", "$[1]", "$[1].m"],
        ),
        (
            "a: 1\nn: 2\nc: {n: 3, d: [4, {e: 5}]}\n",
            &["$.a.n", "$.n", "$.c.n", "$.c.d[1].e", "$.c.d"],
        ),
    ];
    for (document, patterns) in cases {
        for pattern in patterns {
            let [scanned, parsed] = scanned_and_parsed(StructuredMode::Yaml, document, pattern);
            assert_eq!(scanned, parsed, "{pattern} in {document:?}");
        }
    }
}

#[test]
fn yaml_exact_paths_name_the_root_key_not_a_nested_one() {
    let document = "events:\n  - n: 5\n    vip: true\n";
    let paths = |pattern: &str| {
        build_structured_searcher(&[pattern], structured_config(StructuredMode::Yaml))
            .find_structured(document)
            .expect("valid YAML")
            .into_iter()
            .map(|matched| matched.path)
            .collect::<Vec<_>>()
    };

    assert!(paths("$.n").is_empty());
    assert!(paths("$.n=5").is_empty());
    assert_eq!(paths("$.events[0].n=5"), ["$.events[0].n"]);
}

#[test]
fn yaml_flow_mapping_keys_are_not_values() {
    let document = "events:\n  - {vip: true, n: 5}\n";
    let searcher =
        build_structured_searcher(&["vip", "5"], structured_config(StructuredMode::Yaml));

    let found = searcher.find_structured(document).expect("valid YAML");

    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].path.as_str(), found[0].node_text),
        ("$.events[0].n", "5")
    );
}

#[test]
fn structured_json_lines_search_each_value_with_paths_from_its_root() {
    let haystack = "{\"level\":\"info\",\"ms\":12}\n{\"level\":\"error\",\"ms\":3011}\n\n{\"level\":\"error\",\"ms\":40}\n";
    let projection = |patterns: &[&str], limit: Option<usize>| {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .max_matches(limit)
            .build();
        let matches = build_structured_searcher(patterns, config)
            .find_structured(haystack)
            .expect("JSON Lines should parse");
        matches
            .iter()
            .map(|matched| {
                assert_eq!(&haystack[matched.start..matched.end], matched.node_text);
                (matched.path.clone(), matched.node_text)
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(
        projection(&["$.ms > 100", "$.level == \"error\""], None),
        vec![
            ("$.level".to_string(), "\"error\""),
            ("$.ms".to_string(), "3011"),
            ("$.level".to_string(), "\"error\""),
        ]
    );
    assert_eq!(
        projection(&["error"], Some(1)),
        vec![("$.level".to_string(), "\"error\"")]
    );
    assert_eq!(projection(&["$.ms"], None).len(), 3);
}

#[test]
fn yaml_merge_keys_add_only_the_keys_a_mapping_lacks() {
    let document = "\
.defaults: &defaults
  image: ruby:3.3
  retry: 2
.cache: &cache
  retry: 5
  cache: true
test:
  <<: *defaults
  script: rake test
  retry: 0
lint:
  <<: [*defaults, *cache]
  script: rubocop
";
    let hits = |pattern| structured_hits(StructuredMode::Yaml, pattern, document);

    assert_eq!(
        hits("$.test.image"),
        [("$.test.image".into(), "ruby:3.3".into())]
    );
    assert_eq!(hits("$.test.retry"), [("$.test.retry".into(), "0".into())]);
    assert_eq!(hits("$.lint.retry"), [("$.lint.retry".into(), "2".into())]);
    assert_eq!(
        hits("$.lint.cache"),
        [("$.lint.cache".into(), "true".into())]
    );
    assert!(hits(r#"$.test["<<"]"#).is_empty());
    assert_eq!(
        hits("$.retry > 1"),
        [
            (r#"$[".defaults"].retry"#.into(), "2".into()),
            (r#"$[".cache"].retry"#.into(), "5".into()),
            ("$.lint.retry".into(), "2".into()),
        ]
    );
    assert_eq!(
        hits(r#"$.image == "ruby:3.3" AND $.script == "rake test""#),
        [("$.test.image".into(), "ruby:3.3".into())]
    );
    assert_eq!(
        hits("rubocop"),
        [("$.lint.script".into(), "rubocop".into())]
    );
}

#[test]
fn numeric_predicates_compare_large_integers_exactly_and_never_order_nan() {
    let json = r#"{"id":9007199254740993,"ratio":1.0}"#;
    let toml = "id = 9007199254740993\nmissing = nan\n";
    let yaml = "id: 9_007_199_254_740_993\nmissing: .nan\nlimit: .inf\nduration: 1:30\n";

    for (mode, document) in [
        (StructuredMode::Json, json),
        (StructuredMode::Toml, toml),
        (StructuredMode::Yaml, yaml),
    ] {
        let hits = |pattern| structured_hits(mode, pattern, document).len();
        assert_eq!(hits("$.id == 9007199254740993"), 1, "{mode:?}");
        assert_eq!(hits("$.id == 9007199254740992"), 0, "{mode:?}");
        assert_eq!(hits("$.id > 9007199254740992"), 1, "{mode:?}");
        assert_eq!(hits("$.missing >= 0"), 0, "{mode:?}");
        assert_eq!(hits("$.missing <= 0"), 0, "{mode:?}");
    }
    assert_eq!(
        structured_hits(StructuredMode::Json, "$.ratio == 1", json).len(),
        1
    );
    assert_eq!(
        structured_hits(StructuredMode::Toml, "$.missing != 0", toml).len(),
        1
    );
    assert_eq!(
        structured_hits(StructuredMode::Yaml, "$.limit > 1e308", yaml).len(),
        1
    );
    assert_eq!(
        structured_hits(StructuredMode::Yaml, "$.duration == 90", yaml).len(),
        1
    );
}

#[test]
fn json_paths_match_keys_written_with_escapes() {
    let document = r#"{"x":{"eA":1,"a\\b":2,"q\"":3}}"#;
    let hits = |pattern| structured_hits(StructuredMode::Json, pattern, document);

    assert_eq!(hits("$.x.eA"), [("$.x.eA".into(), "1".into())]);
    assert_eq!(
        hits(r#"$.x["a\\b"]"#),
        [(r#"$.x["a\\b"]"#.into(), "2".into())]
    );
    assert_eq!(
        hits(r#"$.x["q\""]"#),
        [(r#"$.x["q\""]"#.into(), "3".into())]
    );
    assert!(hits(r#"$.x["a\\\\b"]"#).is_empty());
}

#[test]
fn structured_printed_paths_with_hostile_quoted_keys_search_again() {
    for key in ["a=b", "a]b", "a\"b", "a.b", "x=\"y\"]"] {
        let quoted = serde_json::to_string(key).expect("keys serialize");
        let documents = [
            (
                StructuredMode::Json,
                format!("{{{quoted}: {{\"v\": \"x\"}}}}"),
            ),
            (StructuredMode::Yaml, format!("{quoted}:\n  v: x\n")),
            (StructuredMode::Toml, format!("[{quoted}]\nv = \"x\"\n")),
        ];
        for (mode, document) in documents {
            for (pattern, path) in [
                (format!("$[{quoted}]"), format!("$[{quoted}]")),
                (format!("$[{quoted}].v"), format!("$[{quoted}].v")),
                (format!("$[{quoted}].v=x"), format!("$[{quoted}].v")),
            ] {
                // A second pattern of another kind takes the parser instead of the scanner.
                for patterns in [vec![pattern.as_str()], vec![pattern.as_str(), "never"]] {
                    let searcher = build_structured_searcher(&patterns, structured_config(mode));
                    let found = searcher
                        .find_structured(&document)
                        .expect("documents parse");
                    let paths = found.iter().map(|found| found.path.as_str());
                    assert_eq!(
                        paths.collect::<Vec<_>>(),
                        [path.as_str()],
                        "{mode:?} {pattern}"
                    );
                }
            }
        }
    }
}

#[test]
fn structured_path_value_patterns_split_at_the_unquoted_equals() {
    let searcher =
        build_structured_searcher(&[r#"$["a=b"]=x"#], structured_config(StructuredMode::Json));

    let found = searcher
        .find_structured(r#"{"a=b": "x", "a": "b\"]=x"}"#)
        .expect("valid JSON");

    let found = found
        .iter()
        .map(|found| (found.path.as_str(), found.match_kind));
    assert_eq!(
        found.collect::<Vec<_>>(),
        [(r#"$["a=b"]"#, StructuredMatchKind::PathValue)]
    );
}

/// Searches each document through the byte scanner, the parser and the predicate paths on a
/// thread with a 64 KiB stack, returning how many matches each found.
fn search_on_small_stack(
    documents: Vec<(StructuredMode, String)>,
) -> Vec<(StructuredMode, [Result<usize, SearchError>; 3])> {
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            documents
                .into_iter()
                .map(|(mode, document)| {
                    let found = |patterns: &[&str]| {
                        build_structured_searcher(patterns, structured_config(mode))
                            .find_structured(&document)
                            .map(|found| found.len())
                    };
                    let found = [found(&["$.q"]), found(&["1", "$.q"]), found(&["$.q > 1"])];
                    (mode, found)
                })
                .collect()
        })
        .expect("the thread starts")
        .join()
        .expect("deep documents fit the stack")
}

fn nested(open: &str, close: &str, depth: usize) -> String {
    format!("{}1{}", open.repeat(depth), close.repeat(depth))
}

#[test]
fn structured_search_fits_small_thread_stacks() {
    let yaml_block = (0..510)
        .map(|depth| {
            let value = if depth == 509 { " 1" } else { "" };
            format!("{}a:{value}\n", "  ".repeat(depth))
        })
        .collect::<String>();
    let dotted = vec!["a"; 3000].join(".");
    // Each document nests to its parser's limit, or far deeper for TOML dotted keys, which
    // have none, and holds a single `1` at the bottom.
    let documents = vec![
        (StructuredMode::Json, nested("{\"a\":", "}", 127)),
        (StructuredMode::Json, nested("[", "]", 127)),
        (StructuredMode::Yaml, yaml_block),
        (StructuredMode::Yaml, nested("[", "]", 510)),
        (StructuredMode::Yaml, nested("{a: ", "}", 510)),
        (
            StructuredMode::Toml,
            format!("a = {}", nested("{b = ", "}", 255)),
        ),
        (
            StructuredMode::Toml,
            format!("a = {}", nested("[", "]", 255)),
        ),
        (StructuredMode::Toml, format!("{dotted} = 1\n")),
        (StructuredMode::Toml, format!("[{dotted}]\nv = 1\n")),
    ];

    let searched = search_on_small_stack(documents);

    for (mode, found) in searched {
        assert_eq!(found, [Ok(0), Ok(1), Ok(0)], "{mode:?}");
    }
}

/// Searches TOML documents whose top table nests `depth` tables, by dotted keys and by a
/// header, for that table on a thread with a 64 KiB stack. Returns each document with the
/// texts matched in it.
fn top_tables_on_small_stack(depth: usize) -> Vec<(String, Result<Vec<String>, SearchError>)> {
    let dotted = vec!["a"; depth].join(".");
    let documents = [format!("{dotted} = 1\n"), format!("[{dotted}]\nv = 1\n")];
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let searcher =
                build_structured_searcher(&["$.a"], structured_config(StructuredMode::Toml));
            documents
                .into_iter()
                .map(|document| {
                    let texts = searcher.find_structured(&document).map(|found| {
                        found
                            .iter()
                            .map(|matched| document[matched.start..matched.end].to_owned())
                            .collect()
                    });
                    (document, texts)
                })
                .collect()
        })
        .expect("the thread starts")
        .join()
        .expect("deep documents fit the stack")
}

#[test]
fn toml_tables_matched_above_deep_nesting_span_it_on_small_thread_stacks() {
    for (document, texts) in top_tables_on_small_stack(3000) {
        // The top table spans every key and value nested under it.
        assert_eq!(texts.expect("valid TOML"), [document.trim_end()]);
    }
}

#[test]
fn toml_tables_matched_above_nesting_past_any_stack_fail_cleanly() {
    // Deep enough that spanning the top table by recursion overflows even a grown stack.
    for (_, texts) in top_tables_on_small_stack(300_000) {
        assert!(
            matches!(texts, Ok(_) | Err(SearchError::StructuredParse { .. })),
            "{texts:?}"
        );
    }
}

#[test]
fn structured_search_past_parser_limits_fails_cleanly_on_small_thread_stacks() {
    let dotted = vec!["a"; 20_000].join(".");
    let documents = vec![
        (StructuredMode::Json, nested("{\"a\":", "}", 5000)),
        (StructuredMode::Json, nested("[", "]", 5000)),
        (StructuredMode::Yaml, nested("[", "]", 5000)),
        (StructuredMode::Yaml, nested("{a: ", "}", 5000)),
        (
            StructuredMode::Toml,
            format!("a = {}", nested("{b = ", "}", 5000)),
        ),
        (
            StructuredMode::Toml,
            format!("a = {}", nested("[", "]", 5000)),
        ),
        (StructuredMode::Toml, format!("{dotted} = 1\n")),
    ];

    let searched = search_on_small_stack(documents);

    for (mode, found) in searched {
        let clean = |result: &Result<usize, SearchError>| {
            matches!(result, Ok(_) | Err(SearchError::StructuredParse { .. }))
        };
        assert!(found.iter().all(clean), "{mode:?}: {found:?}");
    }
}
