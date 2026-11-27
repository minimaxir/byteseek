use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;

use tempfile::{Builder as TempFileBuilder, NamedTempFile};

/// Writes `corpus` to a temporary `.txt` file that is deleted when the handle drops.
pub fn write_corpus(prefix: &str, corpus: &str) -> NamedTempFile {
    let mut file = TempFileBuilder::new()
        .prefix(prefix)
        .suffix(".txt")
        .tempfile()
        .expect("temporary corpus file should be created");
    file.write_all(corpus.as_bytes())
        .expect("temporary corpus should be written");
    file.flush().expect("temporary corpus should be flushed");
    file
}

/// Whether a tool succeeded: status 0, or grep's status 1 for nothing matched, with no output.
pub fn succeeded(status: ExitStatus, stdout: &[u8]) -> bool {
    status.success() || (status.code() == Some(1) && stdout.trim_ascii().is_empty())
}

/// Runs `command` and returns its stdout, panicking if it fails.
pub fn checked_output(mut command: Command, tool: &str) -> Vec<u8> {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to execute {tool}: {error}"));
    assert!(
        succeeded(output.status, &output.stdout),
        "{tool} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    output.stdout
}

/// Builds the release `byteseek` binary once per process and returns its path.
pub fn byteseek_path() -> &'static Path {
    static RUNNER: OnceLock<PathBuf> = OnceLock::new();
    RUNNER
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(["build", "--release", "--bin", "byteseek"])
                .current_dir(manifest_dir.as_path())
                .status()
                .expect("byteseek build should start");
            assert!(status.success(), "byteseek build should succeed");

            let configured_target = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
            let target_dir = match configured_target {
                Some(path) if path.is_absolute() => path,
                Some(path) => manifest_dir.join(path),
                None => manifest_dir.join("target"),
            };
            let executable = target_dir
                .join("release")
                .join(format!("byteseek{}", env::consts::EXE_SUFFIX));
            assert!(
                executable.is_file(),
                "byteseek was not built at {}",
                executable.display()
            );
            executable
        })
        .as_path()
}
