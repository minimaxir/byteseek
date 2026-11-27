from __future__ import annotations

import pickle
import threading
from collections.abc import Callable
from datetime import date, datetime, time, timedelta, timezone
from enum import StrEnum
from pathlib import Path
from typing import Any

import pytest

from byteseek import (
    CaseMode,
    FileMatchResult,
    Match,
    PathOrder,
    SearchConfig,
    Searcher,
    SearchError,
    StructuredFileMatchResult,
    StructuredMatch,
    StructuredMatchKind,
    StructuredMode,
    StructuredPredicateHandling,
)


def test_search_config_defaults() -> None:
    config = SearchConfig()

    assert config.case_mode == CaseMode.SENSITIVE
    assert config.boolean_queries is False
    assert config.whole_word is False
    assert config.overlapping is False
    assert config.ignore_gitignore is False
    assert config.path_order == PathOrder.DISCOVERY
    assert config.max_matches is None
    assert config.max_edit_distance is None
    assert config.structured_mode is None
    assert config.structured_predicate_handling == StructuredPredicateHandling.IGNORE


def test_search_config_sets_every_option() -> None:
    config = SearchConfig(
        case_mode=CaseMode.ASCII_INSENSITIVE,
        boolean_queries=True,
        whole_word=True,
        overlapping=True,
        ignore_gitignore=True,
        path_order=PathOrder.PATH,
        max_matches=4,
        max_edit_distance=1,
        structured_mode=StructuredMode.JSON,
        structured_predicate_handling=StructuredPredicateHandling.ERROR,
    )

    assert config.case_mode is CaseMode.ASCII_INSENSITIVE
    assert config.boolean_queries is True
    assert config.whole_word is True
    assert config.overlapping is True
    assert config.ignore_gitignore is True
    assert config.path_order is PathOrder.PATH
    assert config.max_matches == 4
    assert config.max_edit_distance == 1
    assert config.structured_mode is StructuredMode.JSON
    assert config.structured_predicate_handling is StructuredPredicateHandling.ERROR


@pytest.mark.parametrize(
    ("option", "enum"),
    [
        ("case_mode", CaseMode),
        ("path_order", PathOrder),
        ("structured_mode", StructuredMode),
        ("structured_predicate_handling", StructuredPredicateHandling),
    ],
)
def test_enum_options_round_trip_members_and_their_values(option: str, enum: type[StrEnum]) -> None:
    def configured(value: object) -> object:
        options: dict[str, Any] = {option: value}
        return getattr(SearchConfig(**options), option)

    for member in enum:
        assert configured(member) is member
        assert configured(member.value) is member

    with pytest.raises(ValueError, match="is not a valid"):
        configured("bogus")


def test_negative_limits_raise_value_error() -> None:
    with pytest.raises(ValueError, match="max_matches must be at least 0, not -1"):
        SearchConfig(max_matches=-1)
    with pytest.raises(ValueError, match="max_edit_distance"):
        SearchConfig(max_edit_distance=-2)


def test_search_config_compares_hashes_pickles_and_reprs_by_options() -> None:
    config = SearchConfig(case_mode="ascii_insensitive", max_matches=3)
    same = SearchConfig(max_matches=3, case_mode=CaseMode.ASCII_INSENSITIVE)

    assert config == same and hash(config) == hash(same)
    assert config != SearchConfig()
    assert pickle.loads(pickle.dumps(config)) == config
    assert repr(SearchConfig()) == "SearchConfig()"
    assert repr(config) == "SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE, max_matches=3)"


def test_searcher_accepts_a_string_or_any_iterable_of_strings() -> None:
    assert Searcher("needle").patterns == ("needle",)
    assert Searcher(pattern for pattern in ["a", "b"]).patterns == ("a", "b")
    assert len(Searcher({"x"}).patterns) == 1
    with pytest.raises(TypeError):
        Searcher([1])  # type: ignore[list-item]


def test_searcher_reprs_and_pickles_as_its_patterns_and_options() -> None:
    config = SearchConfig(whole_word=True)
    searcher = Searcher(("error", "timeout"), config)

    assert repr(Searcher(["error"])) == "Searcher(['error'])"
    assert repr(searcher) == "Searcher(['error', 'timeout'], SearchConfig(whole_word=True))"
    restored = pickle.loads(pickle.dumps(searcher))
    assert restored.patterns == searcher.patterns
    assert restored.config == config
    assert restored.find("error: timeout") == searcher.find("error: timeout")


def test_invalid_pattern_inputs_raise() -> None:
    with pytest.raises(SearchError):
        Searcher([])

    with pytest.raises(SearchError):
        Searcher(["abc", ""])


def test_boolean_queries_match_lines_regardless_of_term_order() -> None:
    config = SearchConfig(boolean_queries=True)
    searcher = Searcher(["time AND date"], config)
    lines = searcher.find_lines("time before date\ndate before time\ntime only")

    assert [line.line for line in lines] == ["time before date", "date before time"]


def test_case_sensitive_single_pattern() -> None:
    searcher = Searcher(["cat"])
    matches = searcher.find("cat scatter catalog")

    assert [(match.start, match.end, match.pattern_index) for match in matches] == [
        (0, 3, 0),
        (5, 8, 0),
        (12, 15, 0),
    ]
    first = searcher.find_first("zzz cat zzz")
    assert first is not None and first.start == 4
    assert searcher.contains("catalog")
    assert searcher.count("cat cat cat") == 3


def test_case_insensitive_single_pattern() -> None:
    config = SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE)
    searcher = Searcher(["warn"], config)
    matches = searcher.find("WARN Warn warning wArN")

    assert len(matches) == 4
    assert matches[0].start == 0
    assert matches[1].start == 5
    assert matches[2].start == 10
    assert matches[3].start == 18


def test_case_insensitive_tail_anchor_does_not_panic() -> None:
    config = SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE)
    searcher = Searcher(["warn"], config)
    haystack = ("x" * 5_000) + "W"

    assert searcher.find(haystack) == []


def test_overlapping_and_whole_word() -> None:
    overlapping = Searcher(["ana"], SearchConfig(overlapping=True))
    overlaps = overlapping.find("bananana")
    assert [match.start for match in overlaps] == [1, 3, 5]

    whole_word = Searcher(["cat"], SearchConfig(whole_word=True))
    words = whole_word.find("cat scatter cat catalog")
    assert [match.start for match in words] == [0, 12]


def test_max_matches_is_honored() -> None:
    searcher = Searcher(["a"], SearchConfig(max_matches=2))
    matches = searcher.find("aaaa")

    assert len(matches) == 2
    assert searcher.count("aaaa") == 2


def test_approximate_matching_finds_typos() -> None:
    config = SearchConfig(max_edit_distance=1)
    searcher = Searcher(["ERROR"], config)
    matches = searcher.find("EROR\nERROR\nERRPR")

    assert [(match.start, match.end) for match in matches] == [(0, 4), (5, 10), (11, 16)]
    first = searcher.find_first("xx EROR yy")
    assert first is not None and first.start == 3
    assert searcher.contains("ERRPR")


def test_approximate_matching_respects_case_and_whole_word() -> None:
    config = SearchConfig(
        case_mode=CaseMode.ASCII_INSENSITIVE,
        whole_word=True,
        max_edit_distance=1,
    )
    searcher = Searcher(["warn"], config)
    matches = searcher.find("WARN worn warning swarn")

    assert [(match.start, match.end) for match in matches] == [(0, 4), (5, 9)]


def test_approximate_matching_respects_overlapping_and_line_boundaries() -> None:
    overlap_searcher = Searcher(
        ["anna"], SearchConfig(overlapping=True, max_edit_distance=1)
    )
    overlap_matches = overlap_searcher.find("bananana")
    assert [match.start for match in overlap_matches] == [1, 3, 5]

    line_searcher = Searcher(["ERROR"], SearchConfig(max_edit_distance=1))
    lines = line_searcher.find_lines("ER\nROR\nEROR\n")
    assert [line.line for line in lines] == ["EROR"]


def test_multi_pattern_indexes_follow_patterns() -> None:
    searcher = Searcher(
        ["he", "she", "his", "hers"], SearchConfig(overlapping=True)
    )
    matches = searcher.find("ushers and his")

    assert searcher.patterns == ("he", "she", "his", "hers")
    assert [match.pattern_index for match in matches] == [1, 0, 3, 2]


def test_sensitive_non_overlapping_multi_pattern_ties_and_limits() -> None:
    searcher = Searcher(["a", "ab", "ab"])
    haystack = "ab ab"

    matches = searcher.find(haystack)

    assert [(match.start, match.end, match.pattern_index) for match in matches] == [
        (0, 2, 1),
        (3, 5, 1),
    ]
    first = searcher.find_first(haystack)
    assert first is not None
    assert (first.start, first.end, first.pattern_index) == (0, 2, 1)
    assert searcher.contains(haystack)
    assert searcher.count(haystack) == 2
    assert Searcher(["a", "ab", "ab"], SearchConfig(max_matches=1)).count(
        haystack
    ) == 1


def test_ascii_insensitive_non_overlapping_multi_pattern_apis() -> None:
    config = SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE)
    searcher = Searcher(["ab", "AB", "abc"], config)
    haystack = ("x" * 10) + "ABC" + ("x" * 577) + "ab" + ("x" * 3_504)

    matches = searcher.find(haystack)

    assert [(match.start, match.end, match.pattern_index) for match in matches] == [
        (10, 13, 2),
        (590, 592, 0),
    ]
    first = searcher.find_first(haystack)
    assert first is not None
    assert (first.start, first.end, first.pattern_index) == (10, 13, 2)
    assert searcher.contains(haystack)
    assert searcher.count(haystack) == 2
    assert Searcher(["ab", "AB", "abc"], SearchConfig(
        case_mode=CaseMode.ASCII_INSENSITIVE,
        max_matches=1,
    )).count(haystack) == 1
    assert Searcher(["aab", "xyz"], config).find("a") == []


def test_large_parallel_lines_preserve_python_order_and_offsets() -> None:
    searcher = Searcher(["alpha", "beta"])
    haystack = "alpha beta\nnone\n" * 60_000

    lines = searcher.find_lines(haystack, parallel=True)

    assert len(lines) == 60_000
    assert lines[0].line_number == 1
    assert [(match.start, match.end, match.pattern_index) for match in lines[0].matches] == [
        (0, 5, 0),
        (6, 10, 1),
    ]
    assert lines[-1].line_number == 119_999
    assert [(match.start, match.end) for match in lines[-1].matches] == [(0, 5), (6, 10)]


def test_python_result_objects_preserve_repr_and_immutability() -> None:
    line_match = Searcher(["error"]).find_lines("error found")[0]

    assert repr(line_match) == "LineMatch(line_number=1, line='error found', matches=<1 match>)"
    with pytest.raises(AttributeError):
        line_match.line_number = 2  # type: ignore[misc]

    matches = line_match.matches
    assert isinstance(matches, tuple)
    assert [(match.start, match.end, match.pattern_index) for match in matches] == [(0, 5, 0)]
    assert len(line_match.matches) == 1


def test_search_config_options_are_keyword_only() -> None:
    with pytest.raises(TypeError):
        SearchConfig(CaseMode.ASCII_INSENSITIVE)  # type: ignore[misc]

    config = SearchConfig(
        case_mode=CaseMode.ASCII_INSENSITIVE,
        whole_word=True,
        ignore_gitignore=True,
        max_matches=4,
        max_edit_distance=1,
    )

    assert config.case_mode == CaseMode.ASCII_INSENSITIVE
    assert config.whole_word is True
    assert config.ignore_gitignore is True
    assert config.max_matches == 4
    assert config.max_edit_distance == 1
    assert config.path_order == PathOrder.DISCOVERY


def test_matches_of_one_pattern_share_its_index_object() -> None:
    # Python itself shares the small integers, so the pattern must come after those.
    patterns = [f"pattern{index:03}" for index in range(300)]
    first, second = Searcher(patterns).find("pattern299 pattern299")

    assert first.pattern_index == 299
    assert first.pattern_index is second.pattern_index


def test_file_result_types_are_exported() -> None:
    assert FileMatchResult.__name__ == "FileMatchResult"
    assert StructuredFileMatchResult.__name__ == "StructuredFileMatchResult"
    assert StructuredMatch.__name__ == "StructuredMatch"


def test_line_search_methods_are_equivalent() -> None:
    config = SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE)
    searcher = Searcher(["error", "panic"], config)

    haystack = "ok\nERROR found\nnope\npanic: bad\n"
    sequential = searcher.find_lines(haystack)
    parallel = searcher.find_lines(haystack, parallel=True)

    assert len(sequential) == 2
    assert sequential[0].line_number == 2
    assert sequential[1].line_number == 4

    seq_projection = [
        (
            line_match.line_number,
            line_match.line,
            [(match.start, match.end, match.pattern_index) for match in line_match.matches],
        )
        for line_match in sequential
    ]
    par_projection = [
        (
            line_match.line_number,
            line_match.line,
            [(match.start, match.end, match.pattern_index) for match in line_match.matches],
        )
        for line_match in parallel
    ]
    assert seq_projection == par_projection


def test_streaming_json_predicates_filter_unrelated_fields() -> None:
    searcher = Searcher(
        ["$.latency_ms > 500"], SearchConfig(structured_mode=StructuredMode.JSON)
    )
    document = ('{"events":[{"unused":"not a timestamp","latency_ms":10},'
                '{"unused":null,"latency_ms":900},'
                '{"unused":false,"latency_ms":"slow"}]}')
    matches = searcher.find_structured(document)
    assert [(match.path, match.node_text, match.match_kind) for match in matches] == [
        ("$.events[1].latency_ms", "900", StructuredMatchKind.PREDICATE)
    ]


@pytest.mark.parametrize("limit", [None, 0, 1, 7, 8, 9, 12])
def test_batched_structured_paths_preserve_order_and_limits(
    tmp_path: Path, limit: int | None
) -> None:
    indexes = [9, 0, 4, 2, 20, 8, 1, 5, 3, 6, 7, 9]
    patterns = [f"$.rows[{index}].v" for index in indexes]
    document = '{"rows":[' + ",".join(f'{{"v":{index}}}' for index in range(10)) + "]}"
    searcher = Searcher(patterns, SearchConfig(
        structured_mode=StructuredMode.JSON, max_matches=limit
    ))
    expected = [(pattern_index, patterns[pattern_index], str(index))
                for pattern_index, index in enumerate(indexes) if index < 10]
    if limit is not None:
        expected = expected[:limit]
    matches = searcher.find_structured(document)
    assert [(match.pattern_index, match.path, match.node_text) for match in matches] == expected
    assert all(document[match.start:match.end] == match.node_text for match in matches)
    (tmp_path / "input.json").write_text(document, encoding="utf-8")
    files = searcher.find_structured_path(str(tmp_path))
    assert [(match.pattern_index, match.path, match.node_text)
            for file in files for match in file.matches] == expected


def test_structured_search_exposes_rich_matches(tmp_path: Path) -> None:
    config = SearchConfig(
        structured_mode=StructuredMode.JSON,
        path_order=PathOrder.PATH,
    )
    searcher = Searcher(["$.user.name"], config)
    document = '{"user":{"name":"Alice"}}'

    matches = searcher.find_structured(document)
    assert len(matches) == 1
    assert matches[0].path == "$.user.name"
    assert matches[0].node_text == '"Alice"'
    assert matches[0].match_kind == StructuredMatchKind.PATH

    (tmp_path / "z.json").write_text(document, encoding="utf-8")
    (tmp_path / "a.json").write_text(document, encoding="utf-8")
    file_results = searcher.find_structured_path(str(tmp_path))
    assert [result.path.rsplit("/", 1)[-1] for result in file_results] == [
        "a.json",
        "z.json",
    ]
    assert file_results[0].matches[0].path == "$.user.name"

    malformed = '{"user":'
    for search in (searcher.find, searcher.find_first, searcher.contains, searcher.count):
        with pytest.raises(SearchError, match="failed to parse structured input"):
            search(malformed)


def values_at(mode: StructuredMode, document: str, paths: list[str]) -> dict[str, object]:
    """Searches `document` for `paths` and returns each match's value by path."""
    searcher = Searcher(paths, SearchConfig(structured_mode=mode))
    return {match.path: match.value() for match in searcher.find_structured(document)}


def test_structured_values_read_json_in_document_order() -> None:
    document = '{"z": {"b": [true, null, 2.5, "e\\u0041"], "a": 18446744073709551615}}'

    values = values_at(StructuredMode.JSON, document, ["$.z", "$.z.b[3]"])

    assert values == {
        "$.z": {"b": [True, None, 2.5, "eA"], "a": 18446744073709551615},
        "$.z.b[3]": "eA",
    }
    keys = values["$.z"]
    assert isinstance(keys, dict)
    assert list(keys) == ["b", "a"]


def test_structured_values_read_yaml_as_predicates_do() -> None:
    document = (
        "spec:\n"
        "  mode: 0755\n"
        "  python: 3.10\n"
        "  enabled: yes\n"
        "  timeout: 1:30\n"
        "  owner: ~\n"
        "  quoted: '0755'\n"
        "  items:\n"
        "    - ключ: a\n"
        "      nested:\n"
        "        deep: 1\n"
        "  script: |\n"
        "    echo hi\n"
    )

    values = values_at(
        StructuredMode.YAML, document, ["$.spec.items[0]", "$.spec.script", "$.spec"]
    )
    predicate = Searcher(["$.mode > 700"], SearchConfig(structured_mode=StructuredMode.YAML))

    assert values["$.spec.items[0]"] == {"ключ": "a", "nested": {"deep": 1}}
    assert values["$.spec.script"] == "echo hi\n"
    spec = values["$.spec"]
    assert isinstance(spec, dict)
    assert {key: spec[key] for key in ["mode", "python", "enabled", "timeout", "owner"]} == {
        "mode": 755,
        "python": 3.1,
        "enabled": True,
        "timeout": 90,
        "owner": None,
    }
    assert spec["quoted"] == "0755"
    assert [match.value() for match in predicate.find_structured(document)] == [755]


def test_structured_values_read_toml_tables_and_datetimes() -> None:
    document = (
        'when = 1979-05-27T07:32:00-07:00\n'
        "day = 2024-05-01\n"
        "clock = 07:32:00.123456789\n"
        'features = ["full"]\n'
        "a.b.c = 1\n"
        "a.d = 2\n"
        "\n"
        "[server]\n"
        'host = "h"\n'
        "\n"
        "[other]\n"
        "k = 1\n"
        "\n"
        "[server.tls]\n"
        "on = true\n"
        "\n"
        "[[jobs]]\n"
        'name = "one"\n'
        "\n"
        "[[jobs]]\n"
        'name = "two"\n'
        "[jobs.step]\n"
        'run = "x"\n'
    )

    values = values_at(
        StructuredMode.TOML,
        document,
        ["$.when", "$.day", "$.clock", "$.features", "$.a.b", "$.server", "$.jobs", "$.jobs[1]"],
    )

    assert values == {
        "$.when": datetime(1979, 5, 27, 7, 32, tzinfo=timezone(timedelta(hours=-7))),
        "$.day": date(2024, 5, 1),
        "$.clock": time(7, 32, 0, 123456),
        "$.features": ["full"],
        "$.a.b": {"c": 1},
        "$.server": {"host": "h", "tls": {"on": True}},
        "$.jobs": [{"name": "one"}, {"name": "two", "step": {"run": "x"}}],
        "$.jobs[1]": {"name": "two", "step": {"run": "x"}},
    }


def test_structured_values_of_file_matches_keep_their_indentation(tmp_path: Path) -> None:
    (tmp_path / "deploy.yaml").write_text(
        "spec:\n  replicas: 2\n  template:\n    image: web:1.4\n", encoding="utf-8"
    )
    searcher = Searcher(["$.spec"], SearchConfig(structured_mode=StructuredMode.YAML))

    (result,) = searcher.find_structured_path(tmp_path)

    assert [match.value() for match in result.matches] == [
        {"replicas": 2, "template": {"image": "web:1.4"}}
    ]


def test_structured_values_raise_when_text_needs_the_rest_of_its_document() -> None:
    document = "base: &base\n  retries: 3\njob:\n  <<: *base\n  name: build\n"
    searcher = Searcher(["$.job"], SearchConfig(structured_mode=StructuredMode.YAML))

    (match,) = searcher.find_structured(document)

    with pytest.raises(ValueError, match="undefined alias"):
        match.value()


@pytest.mark.parametrize(
    ("document", "path", "expected"),
    [
        # Dotted keys name a table from the table holding it, which can repeat its keys.
        ("[a.x]\na.k = 1\na.x.a.z = 2\n", "$.a.x.a", {"k": 1, "x": {"a": {"z": 2}}}),
        ("[a]\na.k = 1\n", "$.a.a", {"k": 1}),
        ("a.a.k = 1\n", "$.a.a", {"k": 1}),
        # Text starting its line with `[` is a header or an array element.
        ('x = [ # c\n[["x"]]\n]\n', "$.x[0]", [["x"]]),
        ('x = [\n  ["x"],\n]\n', "$.x[0]", ["x"]),
        ("[[x]]\n", "$.x[0]", {}),
        ('["x"]\n', "$.x", {}),
        ("t = { a.b = 1, c = [\n  [2],\n] }\n", "$.t.a", {"b": 1}),
    ],
)
def test_structured_values_read_toml_text_that_reads_two_ways_by_its_path(
    document: str, path: str, expected: object
) -> None:
    assert values_at(StructuredMode.TOML, document, [path]) == {path: expected}


def test_structured_values_refuse_what_python_cannot_hold() -> None:
    laughs = "root:\n  a: &a [x,x,x,x,x,x,x,x,x]\n" + "".join(
        f"  {name}: &{name} [{', '.join([f'*{previous}'] * 9)}]\n"
        for previous, name in zip("abcdefgh", "bcdefghi")
    )
    (expansion,) = Searcher(
        ["$.root"], SearchConfig(structured_mode=StructuredMode.YAML)
    ).find_structured(laughs)
    times = Searcher(
        ["$.a", "$.b"], SearchConfig(structured_mode=StructuredMode.TOML)
    ).find_structured("a = 0000-01-01\nb = 1990-12-31T23:59:60Z\n")

    with pytest.raises(ValueError, match="too many"):
        expansion.value()
    for match in times:
        with pytest.raises(ValueError):
            match.value()


def run_with_small_stack(work: Callable[[], object]) -> object:
    """Runs `work` on a thread with a 64 KiB stack and returns its result."""
    results: list[object] = []
    previous = threading.stack_size(64 * 1024)
    try:
        thread = threading.Thread(target=lambda: results.append(work()))
        thread.start()
        thread.join()
    finally:
        threading.stack_size(previous)
    return results[0]


@pytest.mark.parametrize(
    ("mode", "document", "pattern"),
    [
        (
            StructuredMode.YAML,
            "".join("  " * level + "a:\n" for level in range(510)) + " " * 1020 + "b: 1\n",
            "$",
        ),
        (StructuredMode.YAML, "a: " + "[" * 510 + "]" * 510 + "\n", "$.a"),
        (StructuredMode.TOML, "a = " + "{b = " * 255 + "1" + "}" * 255 + "\n", "$.a"),
        (StructuredMode.TOML, ".".join(["a"] * 3000) + " = 1\n", "$.a"),
        (StructuredMode.JSON, '{"a": ' + "[" * 126 + "]" * 126 + "}", "$.a"),
    ],
)
def test_deep_documents_search_and_read_on_small_thread_stacks(
    mode: StructuredMode, document: str, pattern: str
) -> None:
    # The second pattern takes the parser instead of the byte scanner.
    searcher = Searcher([pattern, "never"], SearchConfig(structured_mode=mode))

    def search_and_read() -> object:
        (match,) = searcher.find_structured(document)
        return match.value()

    assert run_with_small_stack(search_and_read) is not None


def test_find_path_respects_gitignore_by_default(tmp_path: Path) -> None:
    (tmp_path / ".gitignore").write_text("ignored.txt\n", encoding="utf-8")
    (tmp_path / "visible.txt").write_text("needle here", encoding="utf-8")
    (tmp_path / "ignored.txt").write_text("needle hidden", encoding="utf-8")

    searcher = Searcher(["needle"])
    results = searcher.find_path(str(tmp_path))

    assert [result.path.rsplit("/", 1)[-1] for result in results] == ["visible.txt"]
    assert isinstance(results[0].matches, tuple)
    assert [(match.start, match.end) for match in results[0].matches] == [(0, 6)]


def test_find_path_can_ignore_gitignore_rules(tmp_path: Path) -> None:
    (tmp_path / ".gitignore").write_text("ignored.txt\n", encoding="utf-8")
    (tmp_path / "visible.txt").write_text("needle here", encoding="utf-8")
    (tmp_path / "ignored.txt").write_text("needle hidden", encoding="utf-8")

    config = SearchConfig(ignore_gitignore=True)
    searcher = Searcher(["needle"], config)
    results = searcher.find_path(str(tmp_path))

    assert sorted(result.path.rsplit("/", 1)[-1] for result in results) == [
        "ignored.txt",
        "visible.txt",
    ]


def test_find_path_passes_over_files_that_are_not_utf8(tmp_path: Path) -> None:
    (tmp_path / "notes.txt").write_text("needle here", encoding="utf-8")
    (tmp_path / "image.bin").write_bytes(b"needle \xff\xfe")
    searcher = Searcher(["needle"])

    results = searcher.find_path(tmp_path)

    assert [Path(result.path).name for result in results] == ["notes.txt"]


def test_find_path_rejects_a_named_file_that_is_not_utf8(tmp_path: Path) -> None:
    (tmp_path / "image.bin").write_bytes(b"needle \xff\xfe")
    searcher = Searcher(["needle"])

    with pytest.raises(SearchError):
        searcher.find_path(tmp_path / "image.bin")


def test_search_error_is_named_for_the_package_and_survives_pickling() -> None:
    with pytest.raises(SearchError) as raised:
        Searcher([])

    assert SearchError.__module__ == "byteseek"
    assert Searcher.__module__ == "byteseek"
    assert Match.__module__ == "byteseek"
    restored = pickle.loads(pickle.dumps(raised.value))
    assert type(restored) is SearchError
    assert str(restored) == str(raised.value)


def test_paths_may_be_path_objects_and_structured_fields_are_shared(tmp_path: Path) -> None:
    (tmp_path / "data.json").write_text('{"name": "alice"}', encoding="utf-8")
    (tmp_path / "notes.txt").write_text("alice was here\n", encoding="utf-8")

    by_path = Searcher(["alice"]).find_path(tmp_path)
    by_text = Searcher(["alice"]).find_path(str(tmp_path))
    config = SearchConfig(structured_mode=StructuredMode.JSON)
    structured = Searcher(["$.name"], config).find_structured_path(tmp_path / "data.json")

    assert sorted(result.path for result in by_path) == sorted(result.path for result in by_text)
    assert len(by_path) == 2
    assert [result.path for result in structured] == [str(tmp_path / "data.json")]
    matched = structured[0].matches[0]
    assert (matched.path, matched.node_text) == ("$.name", '"alice"')
    # Fields are built once, so reading one twice gives the same object.
    assert matched.node_text is matched.node_text
    assert repr(matched).startswith("StructuredMatch(start=9, end=16, pattern_index=0")



def test_results_compare_and_hash_by_their_fields(tmp_path: Path) -> None:
    searcher = Searcher(["error"])
    (tmp_path / "log.txt").write_text("an error\n", encoding="utf-8")
    (tmp_path / "data").mkdir()
    (tmp_path / "data" / "a.json").write_text('{"a": 1}', encoding="utf-8")
    structured = Searcher(["$.a"], SearchConfig(structured_mode="json"))

    for search in (
        lambda: searcher.find("an error"),
        lambda: searcher.find_lines("an error"),
        lambda: searcher.find_path(tmp_path),
        lambda: structured.find_structured('{"a": 1}'),
        lambda: structured.find_structured_path(tmp_path / "data"),
    ):
        first, again = search(), search()
        assert first == again
        assert {hash(result) for result in first} == {hash(result) for result in again}
    assert searcher.find("an error") != Searcher(["an"]).find("an error")
    assert searcher.find("an error")[0] != (3, 8, 0)  # type: ignore[comparison-overlap]


def test_results_support_match_statements() -> None:
    match Searcher(["error"]).find_first("an error"):
        case Match(start, end, pattern_index):
            assert (start, end, pattern_index) == (3, 8, 0)
        case _:
            pytest.fail("Match did not destructure")

    structured = Searcher(["$.a"], SearchConfig(structured_mode="json"))
    match structured.find_structured('{"a": 1}'):
        case [StructuredMatch(path="$.a", match_kind=StructuredMatchKind.PATH) as found]:
            assert found.value() == 1
        case _:
            pytest.fail("StructuredMatch did not destructure")


def test_missing_paths_raise_file_not_found(tmp_path: Path) -> None:
    missing = tmp_path / "missing"
    structured = Searcher(["$.a"], SearchConfig(structured_mode=StructuredMode.JSON))

    with pytest.raises(FileNotFoundError) as raised:
        Searcher(["x"]).find_path(missing)
    assert raised.value.filename == str(missing)
    with pytest.raises(FileNotFoundError):
        structured.find_structured_path(missing)


def test_search_error_is_a_value_error() -> None:
    assert issubclass(SearchError, ValueError)


def test_public_methods_have_docstrings() -> None:
    for owner in (Searcher, SearchConfig, Match, StructuredMatch):
        assert owner.__doc__
    for method in ("find", "find_first", "contains", "count", "find_lines", "find_path",
                   "find_structured", "find_structured_path", "patterns", "config"):
        assert getattr(Searcher, method).__doc__, method
