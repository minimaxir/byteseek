//! End-to-end checks of the `byteseek` binary's ripgrep-compatible behavior.
#![cfg(feature = "cli")]

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

/// A directory tree with an ignored file, a hidden file, and a binary file. The empty `.git`
/// directory makes `.gitignore` apply, as it would in a repository.
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let write = |relative: &str, contents: &[u8]| {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().expect("fixture paths have parents"))
            .expect("fixture directories should be created");
        fs::write(path, contents).expect("fixture file should be written");
    };
    fs::create_dir(root.path().join(".git")).expect("fixture .git should be created");
    write(".gitignore", b"ignored.log\n");
    write("app.log", b"INFO ready\nERROR failed\nerror lower\n");
    write("src/lib.rs", b"// ERROR in code\nfn ok() {}\n");
    write("ignored.log", b"ERROR ignored\n");
    write(".hidden/secret.log", b"ERROR hidden\n");
    write("blob.bin", b"ERROR\0binary\n");
    root
}

fn byteseek(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_byteseek"))
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .expect("byteseek should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be UTF-8")
}

#[test]
fn searches_the_current_directory_like_ripgrep() {
    let root = fixture();

    let output = byteseek(root.path(), &["--sort", "path", "ERROR"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        "app.log:ERROR failed\nsrc/lib.rs:// ERROR in code\n"
    );
}

/// Sets the modification time of the fixture file at `relative` to `seconds` after the epoch.
fn set_modified(root: &Path, relative: &str, seconds: u64) {
    File::options()
        .write(true)
        .open(root.join(relative))
        .and_then(|file| file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)))
        .expect("the modification time should be set");
}

#[test]
fn recent_sort_prints_the_latest_change_first_and_is_the_pretty_default() {
    let root = fixture();
    fs::write(root.path().join("src/main.rs"), "// ERROR in main\n").expect("file is written");
    // The fixture's `.git` is empty, so every file is ordered by its modification time.
    set_modified(root.path(), "app.log", 1_000);
    set_modified(root.path(), "src/main.rs", 2_000);
    set_modified(root.path(), "src/lib.rs", 2_000);

    let files = byteseek(root.path(), &["--sort", "recent", "-l", "ERROR"]);
    let lines = byteseek(root.path(), &["--sort", "recent", "-j1", "ERROR"]);
    let pretty = byteseek(root.path(), &["-p", "-l", "ERROR"]);

    assert_eq!(stdout(&files), "src/lib.rs\nsrc/main.rs\napp.log\n");
    assert_eq!(
        stdout(&lines),
        "src/lib.rs:// ERROR in code\nsrc/main.rs:// ERROR in main\napp.log:ERROR failed\n"
    );
    assert!(
        stdout(&pretty).starts_with("src/lib.rs\nsrc/main.rs\napp.log\n\n"),
        "{}",
        stdout(&pretty)
    );
}

#[test]
fn filters_lift_with_flags() {
    let root = fixture();

    let output = byteseek(root.path(), &["--sort", "path", "-uu", "-l", "ERROR"]);

    assert_eq!(
        stdout(&output),
        ".hidden/secret.log\napp.log\nignored.log\nsrc/lib.rs\n"
    );
}

#[test]
fn named_files_print_without_paths_and_binary_files_print_a_notice() {
    let root = fixture();

    let lines = byteseek(root.path(), &["-n", "-i", "error", "app.log"]);
    let binary = byteseek(root.path(), &["ERROR", "blob.bin"]);

    assert_eq!(stdout(&lines), "2:ERROR failed\n3:error lower\n");
    assert_eq!(
        stdout(&binary),
        "binary file matches (found \"\\0\" byte around offset 5)\n"
    );
}

#[test]
fn columns_and_nul_terminated_paths_follow_ripgrep() {
    let root = fixture();

    let columns = byteseek(root.path(), &["--sort", "path", "--column", "ERROR"]);
    let paths = byteseek(root.path(), &["--sort", "path", "-0", "-l", "ERROR"]);

    assert_eq!(
        stdout(&columns),
        "app.log:2:1:ERROR failed\nsrc/lib.rs:1:4:// ERROR in code\n"
    );
    assert_eq!(stdout(&paths), "app.log\0src/lib.rs\0");
}

#[test]
fn exit_status_follows_ripgrep() {
    let root = fixture();

    let missing = byteseek(root.path(), &["nothing-matches-this"]);
    let error = byteseek(root.path(), &["ERROR", "no-such-file"]);
    let regex = byteseek(root.path(), &["ERR.R"]);

    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert_eq!(error.status.code(), Some(2));
    assert_eq!(regex.status.code(), Some(2));
    assert_eq!(
        byteseek(root.path(), &["-F", "ERR.R"]).status.code(),
        Some(1)
    );
}

#[test]
fn reads_piped_stdin_without_paths() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_byteseek"))
        .args(["-c", "ERROR"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("byteseek should start");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"ERROR one\nok\nERROR two\n")
        .expect("stdin should be written");

    let output = child.wait_with_output().expect("byteseek should finish");

    assert_eq!(stdout(&output), "2\n");
}

#[test]
fn structured_search_prints_document_paths() {
    let root = fixture();
    fs::write(
        root.path().join("event.json"),
        br#"{"user":{"name":"alice"},"latency_ms":912}"#,
    )
    .expect("JSON should be written");

    let output = byteseek(
        root.path(),
        &["--parse", "json", "$.latency_ms > 500", "event.json"],
    );

    assert_eq!(stdout(&output), "$.latency_ms:912\n");
}

#[test]
fn structured_equality_predicates_need_spaces_or_double_equals() {
    let root = fixture();
    fs::write(
        root.path().join("users.json"),
        br#"[{"user":{"name":"alice","vip":true}},{"user":{"name":"bob","vip":false}}]"#,
    )
    .expect("JSON should be written");
    let search = |pattern| {
        stdout(&byteseek(
            root.path(),
            &["--parse", "json", pattern, "users.json"],
        ))
    };

    assert_eq!(search("$.user.vip == true"), "$[0].user.vip:true\n");
    assert_eq!(search("$.user.name = \"bob\""), "$[1].user.name:\"bob\"\n");
    assert_eq!(search("$.user.vip=true"), "");
    assert_eq!(search("$[1].user.name=bob"), "$[1].user.name:\"bob\"\n");
}

#[test]
fn pretty_output_heads_matches_and_sums_up_without_escapes_when_piped() {
    let root = fixture();
    // 2026-03-18 10:15:00 UTC. The fixture's `.git` is empty, so no file has a commit.
    for relative in ["app.log", "src/lib.rs"] {
        set_modified(root.path(), relative, 1_773_828_900);
    }

    let output = Command::new(env!("CARGO_BIN_EXE_byteseek"))
        .args(["-p", "ERROR"])
        .current_dir(root.path())
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .output()
        .expect("byteseek should run");
    let text = stdout(&output);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        text.starts_with(
            "app.log  1 line \u{b7} modified 2026-03-18 10:15\n2 \u{258e} ERROR failed\n\n\
             src/lib.rs  1 line \u{b7} modified 2026-03-18 10:15\n\
             1 \u{258e} // ERROR in code\n\n\
             2 matches \u{b7} 2 of 2 files \u{b7} 64 B in "
        ),
        "{text}"
    );
    assert!(!text.contains('\x1b'));
}

#[test]
fn usage_and_file_errors_are_labeled_on_stderr() {
    let root = fixture();

    let missing_pattern = byteseek(root.path(), &[]);
    let missing_file = byteseek(root.path(), &["-p", "ERROR", "app.log", "missing.txt"]);
    let stderr = |output: &Output| String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(missing_pattern.status.code(), Some(2));
    assert!(
        stderr(&missing_pattern).starts_with("error: no pattern given\n\nUsage: byteseek"),
        "{}",
        stderr(&missing_pattern)
    );
    assert_eq!(missing_file.status.code(), Some(2));
    assert!(stderr(&missing_file).starts_with("error: missing.txt: "));
    assert!(stdout(&missing_file).contains(" \u{b7} 1 error \u{b7} 1 of 1 file \u{b7} "));
}

/// One service described in each structured format, beside text that mentions the same key.
fn structured_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    for (relative, contents) in [
        ("api.json", "{\"name\": \"api\", \"replicas\": 3}\n"),
        (
            "events.jsonl",
            "{\"name\": \"deploy\"}\n{\"name\": \"rollback\"}\n",
        ),
        ("web.yml", "name: web\nreplicas: 1\n"),
        ("worker.toml", "name = \"worker\"\nreplicas = 2\n"),
        ("notes.txt", "name: notes\n"),
    ] {
        fs::write(root.path().join(relative), contents).expect("fixture file should be written");
    }
    root
}

#[test]
fn structured_patterns_parse_each_file_in_the_format_its_extension_names() {
    let root = structured_fixture();

    let output = byteseek(root.path(), &["--sort", "path", "$.replicas >= 2"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        "api.json:$.replicas:3\nworker.toml:$.replicas:2\n"
    );
    let lines = byteseek(root.path(), &["--sort", "path", "$.name"]);
    assert_eq!(
        stdout(&lines),
        "api.json:$.name:\"api\"\nevents.jsonl:$.name:\"deploy\"\nevents.jsonl:$.name:\"rollback\"\n\
         web.yml:$.name:web\nworker.toml:$.name:\"worker\"\n"
    );
}

#[test]
fn parse_walks_only_files_of_its_format_unless_globs_choose() {
    let root = structured_fixture();

    let yaml = byteseek(
        root.path(),
        &["--sort", "path", "--parse", "yaml", "-c", "$.name"],
    );
    let chosen = byteseek(root.path(), &["--parse", "yaml", "-g", "*.txt", "$.name"]);

    assert_eq!(yaml.status.code(), Some(0));
    assert_eq!(stdout(&yaml), "web.yml:1\n");
    assert_eq!(stdout(&chosen), "notes.txt:$.name:notes\n");
}

#[test]
fn structured_patterns_need_a_format_for_files_without_a_known_extension() {
    let root = structured_fixture();

    let output = byteseek(root.path(), &["$.name", "notes.txt"]);
    let literal = byteseek(root.path(), &["-F", "-c", "$.name", "notes.txt"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("pass --parse"));
    assert_eq!(literal.status.code(), Some(1));
}

#[test]
fn strict_predicates_work_with_inferred_formats_and_need_structured_patterns() {
    let root = structured_fixture();

    let strict = byteseek(root.path(), &["--strict", "$.name > 1", "api.json"]);
    let plain = byteseek(root.path(), &["--strict", "name", "notes.txt"]);

    assert_eq!(strict.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&strict.stderr).contains("numeric"));
    assert_eq!(plain.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&plain.stderr).contains("--strict needs structured patterns"));
}
