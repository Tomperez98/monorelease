//! Running the binary under test.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::Error;

/// Run a command to completion, capturing both streams and never inheriting a
/// terminal: the release gates assert on what a command printed, so its output
/// must not interleave with ours.
pub fn capture(program: &Path, args: &[&str]) -> Result<Output, Error> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("{} {}", program.display(), args.join(" ")),
            source,
        })
}

/// Where a command's output goes when it fails: indented, after our own line,
/// so the failing command is still obvious in a CI log.
pub fn diagnostics(output: &Output) -> String {
    let mut text = String::new();
    for stream in [&output.stdout, &output.stderr] {
        let stream = String::from_utf8_lossy(stream);
        for line in stream.lines().filter(|line| !line.trim().is_empty()) {
            text.push_str("    ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.trim_end().to_owned()
}
