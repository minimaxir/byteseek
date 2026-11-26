# byteseek

> [!CAUTION]
> This project is being open-sourced earlier than typical in order to transparently demonstrate the power of Opus 5.5/Fable 5.2 in creating software which is faster than state-of-the-art implementations. It has not been fully tested on robust real-world data...yet.

byteseek is modern reimagining of `grep` for logs, code, and structured data, with a command-line tool that is a drop-in replacement for [ripgrep](https://github.com/BurntSushi/ripgrep)

`byteseek` can also parse JSON, YAML, and TOML files and search them by [jq](https://jqlang.org)-style path, by value, or with predicates such as `$.payload.latency_ms > 1000`, which is handy for configs and API dumps where a line-based `grep` loses track of what a value means.

More useful features of `byteseek`:

- **Fuzzy matching** with `-k/--max-errors`, which finds text within N typos (insertions, deletions, or substitutions) of a pattern. A search for `timeout` also catches the `timout` someone wrote at 3 AM.
- **Boolean predicate queries** with `--query`, which select lines that contain every required term in any order, such as `'ERROR AND payments AND NOT retrying'`.
- **Python bindings** with full type hints that release the GIL while searching, so searches from multiple threads run in parallel. See the [Jupyter Notebook](notebooks/byteseek_demo.ipynb) for more demos.
- **Strong performance** at a low memory footprint: on ripgrep's own benchmark corpora, the `byteseek` CLI is 1.1x to 7.5x faster than `rg` end to end with about half of its peak memory, and the Python bindings exceeding optimized packages such as pyahocorsick and orjson. ([Benchmarks](#performance))

The [notebooks folder](notebooks) contains a Jupyter Notebook tour of the Python API.

## Installation

The CLI installs from crates.io with Cargo (Rust 1.96+):

```sh
cargo install byteseek
```

In Python:

```sh
pip3 install byteseek
```

```sh
uv pip install byteseek
```

The Python package also installs the full `byteseek` command line, compiled into the package itself, so `byteseek` (or `python -m byteseek`) works without Cargo and takes the same flags as the native binary.

## Demo

Run `byteseek` with a pattern to search every file under the current directory, skipping hidden files, binary files, and anything matched by a `.gitignore`. Any `ripgrep` command that uses literal patterns works unchanged, including `-e` for multiple patterns, `-i/-S` for case, `-w/-x` for whole words and lines, `-A/-B/-C` for context, `-c/-l/-o` for counts, files, and matches, `-g/-t` for globs and file types, and `--column/-0` for editors and `xargs`:

The output is similar to `ripgrep/rg`'s, byte for byte, down to its colors and the order files print in, as their searches finish: this search through the source of [axum](https://github.com/tokio-rs/axum) for how it limits request bodies is the same one shown with `--pretty` at the top of this README, which lists files the most recently changed first instead. `--sort recent` orders plain output that way too, and `--sort path` orders it by path, as in `rg`.

## Structured Search

A pattern starting with `$.` or `$[` parses each file as a document in the format its extension names: `.json`, `.jsonl`, or `.ndjson` for JSON, `.yaml` or `.yml` for YAML, and `.toml` for TOML. Directories are searched for those files only. `--parse json`, `--parse yaml`, or `--parse toml` picks the format instead, such as for a file with another extension or for stdin. Every pattern then matches one of three ways:

- Plain text matches inside scalar values, such as `failed`.
- A pattern starting with `$` matches a path exactly, such as `$.replicas`, or a path with the value written after an `=` without spaces, such as `$.service=payments`.
- A predicate compares values with `==` (or `=` with spaces around it), `!=`, `>`, `>=`, `<`, `<=`, or `BETWEEN 100 AND 1000`, and clauses join with `AND`. A predicate's path matches at any depth and its clauses must hold within the same object, so `$.payload.latency_ms > 1000` checks every event's `payload`.

Each match prints as `PATH:VALUE`, here listing the React and TypeScript versions that each package of a monorepo depends on:

With `--pretty`, each file's paths and values line up in columns:

Predicates compare numbers, strings, booleans, and RFC 3339 timestamps, and skip values that are null or of another type; `--strict` makes those an error instead. A quoted literal is always a string or a timestamp, so `== "2018"` matches the string `"2018"` and not the number `2018`.

JSON Lines are searched like `jq` reads them: each value is its own document whose paths start at `$`, so a log with one event per line can be filtered with `'$.level == "error" AND $.latency_ms > 1000'`.

## Pretty Output

`-p/--pretty` is opt-in as its layout differs from `ripgrep`'s and it is optimized for better human readbility. JSON/YAML/TOML syntax highlighting will be displayed for those searches.

## Fuzzy and Boolean Predicate Search

`-k N` also matches text within N edits of each pattern, which catches typos and OCR errors that an exact search misses. `--query` treats each pattern as a boolean query over the terms in a line, in any order, and highlights the terms that made the line match:

A line selected only through `NOT`, such as by `NOT INFO`, has no terms to show, so the whole line is its match. Without `--pretty`, lines print in `ripgrep`'s format:

```
$ byteseek --query 'ERROR AND NOT retrying' logs
logs/worker.log:09:14:31 worker-2 ERROR job 4415 timout waiting for smtp relay
logs/api.log:2026-11-21T09:14:09Z ERROR request_id=a822 method=POST path=/payments status=500 latency_ms=3011 upstream timeout from payments-gateway
logs/api.log:2026-11-21T09:14:12Z ERROR request_id=a824 method=POST path=/payments status=502 latency_ms=120 bad gateway: connection reset by peer
```

## In-Memory APIs

### Python

A `Searcher` compiles its patterns once and searches strings, files, or whole directories. Match offsets are UTF-8 byte offsets, and `pattern_index` says which pattern matched.

```py3
from pathlib import Path

from byteseek import CaseMode, SearchConfig, Searcher

searcher = Searcher(["error", "timeout"], SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE))

text = Path("logs/worker.log").read_text()
for line in searcher.find_lines(text):
    print(line.line_number, line.line)

searcher.count(text)  # 3
searcher.contains("all good")  # False

for result in searcher.find_path("src"):
    print(result.path, len(result.matches))
```

`SearchConfig` also takes options such as `whole_word`, `max_matches`, `max_edit_distance` for fuzzy matching, and `boolean_queries`. Enum options also accept their string values, such as `structured_mode="json"`. For structured search, set `structured_mode`:

```py3
from byteseek import StructuredMode

events = Searcher(
    ["failed", "$.payload.latency_ms > 1000"],
    SearchConfig(structured_mode=StructuredMode.JSON),
)
for result in events.find_structured_path("data"):
    for match in result.matches:
        print(match.path, match.node_text)
```

```
$.events[1].payload.latency_ms 3011
$.events[1].payload.status "failed"
$.events[4].payload.latency_ms 2200
$.events[4].payload.status "failed"
```

[Jupyter Notebook with a tour of the Python API](notebooks/byteseek_demo.ipynb).

### Rust

The same `Searcher` is available as a Rust library, along with its reports. Turning off the default `cli` feature leaves out the command-line tool's dependencies:

```toml
[dependencies]
byteseek = { version = "0.1", default-features = false }
```

```rust
use byteseek::{CaseMode, SearchConfig, Searcher};

let config = SearchConfig::builder()
    .case_mode(CaseMode::AsciiInsensitive)
    .build();
let searcher = Searcher::new(["error", "timeout"], config)?;

for line in searcher.find_lines(&text) {
    println!("{}: {}", line.line_number, line.line);
}

let report = searcher.report_path("logs")?;
println!("{}", report.render_top_files_table(10));
```

## Performance

These results are for ripgrep's own [benchsuite](https://github.com/BurntSushi/ripgrep/tree/master/benchsuite) searches on its English subtitles corpus, comparing the `byteseek` and `rg` (15.2.0) command-line tools end to end on an Apple M3 Pro: each run starts a process, reads the file, and writes a count of matching lines.

| Search                  | `byteseek` | `rg`     | Speedup | Peak memory, `byteseek` / `rg` |
| ----------------------- | ---------- | -------- | ------- | ------------------------------ |
| literal                 | 113 ms     | 152 ms   | 1.34x   | 2.7 / 5.1 MiB                  |
| literal, ignore case    | 136 ms     | 262 ms   | 1.93x   | 2.7 / 5.3 MiB                  |
| literal, whole word     | 126 ms     | 154 ms   | 1.22x   | 2.6 / 5.2 MiB                  |
| alternates              | 178 ms     | 262 ms   | 1.47x   | 2.8 / 5.2 MiB                  |
| alternates, ignore case | 208 ms     | 1,566 ms | 7.52x   | 2.9 / 5.6 MiB                  |

On the benchsuite's Russian subtitles and Linux kernel corpora, `byteseek` is 1.14x to 2.10x faster. As a library searching text already in memory, `byteseek` ties `ripgrep`'s searcher on the benchsuite's whole-word searches and is 1.3x to 13.6x faster on the rest. ([Methodology](RIPGREP_COMPARISON.md))

## AI Disclosure

`byteseek` was mostly made using Claude Opus 5.5 and Fable 5.2 with checks and balances as an experiment into how far an agent can push a search engine's performance at the instruction level against `ripgrep`, one of the most heavily optimized command-line tools there is.

For any pull requests that are submitted to this repo (particularly by agents), please manually review the output and ensure any changes are high signal and precisely scoped: both a) low-effort PRs that cause more problems than they solve and b) nontechnical discussions about the use of agentic development may be closed without comment.

## Miscellaneous Notes

- The terminal screenshots were made using [vhs](https://github.com/charmbracelet/vhs) with an additional postprocessing step that crops each screenshot to only its text.

## To Be Added

- More tests with real-world data
- More feature parity with ripgrep (still deciding on which important features are NYI)

## Maintainer/Creator

Max Woolf ([@minimaxir](https://minimaxir.com))

_Max's open-source projects are supported by his [Patreon](https://www.patreon.com/minimaxir) and [GitHub Sponsors](https://github.com/sponsors/minimaxir). If you found this project helpful, any monetary contributions to the Patreon are appreciated and will be put to good creative use._

## License

MIT

The MIT LICENSE for ripgrep is also included as its base benchmarking suite was used for calibration: no algorithmic code from ripgrep is intentionally implemented.
