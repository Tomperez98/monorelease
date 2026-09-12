//! Cross-platform test fixture for `mono`'s own test suite.
//!
//! The tests need a child process that writes files, prints bytes, exits with
//! a chosen code, and spawns descendants. Shell snippets (`sh -c` / `cmd /C`)
//! express that concisely but only on one platform each, so the suite used to
//! be split with `#[cfg(unix)]` and Windows ran almost none of it. This binary
//! implements the handful of effects the tests need in plain Rust, so the same
//! task command works on every target and the behavior is actually verified
//! everywhere.
//!
//! It follows the repository's failure policy: a bad invocation is a bug and
//! exits `3` loudly; a test-controlled refusal exits with the code the test
//! asked for.
//!
//! Usage: `mono-fixture <verb> [args...]`

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// A bad fixture invocation is a broken test, not an expected failure.
const FIXTURE_USAGE_ERROR: i32 = 3;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("mono-fixture: {error}");
            std::process::exit(FIXTURE_USAGE_ERROR);
        }
    }
}

fn run(args: &[String]) -> io::Result<i32> {
    let Some((verb, rest)) = args.split_first() else {
        return Err(usage("a verb is required"));
    };

    match verb.as_str() {
        "print" => {
            write_stdout(required(rest, 0, "print <text>")?)?;
            Ok(0)
        }
        "streams" => {
            write_stdout(required(rest, 0, "streams <stdout> <stderr>")?)?;
            write_stderr(required(rest, 1, "streams <stdout> <stderr>")?)?;
            Ok(0)
        }
        "fail" => Ok(parse_code(required(rest, 0, "fail <code>")?)?),
        "fail-with" => {
            write_stderr(required(rest, 1, "fail-with <code> <stderr>")?)?;
            Ok(parse_code(required(rest, 0, "fail-with <code> <stderr>")?)?)
        }
        "delay-print" => {
            write_stdout(required(rest, 0, "delay-print <first> <millis> <second>")?)?;
            io::stdout().flush()?;
            sleep_millis(required(rest, 1, "delay-print <first> <millis> <second>")?)?;
            write_stdout(required(rest, 2, "delay-print <first> <millis> <second>")?)?;
            Ok(0)
        }
        "write" => {
            let path = required(rest, 0, "write <path> <text>")?;
            let text = required(rest, 1, "write <path> <text>")?;
            write_file(Path::new(path), text)?;
            Ok(0)
        }
        "write-many" => {
            if rest.len() % 2 != 0 {
                return Err(usage("write-many <path> <text> [<path> <text>...]"));
            }
            for pair in rest.as_chunks::<2>().0 {
                write_file(Path::new(&pair[0]), &pair[1])?;
            }
            Ok(0)
        }
        "copy" => {
            let source = required(rest, 0, "copy <source> <destination>")?;
            let destination = required(rest, 1, "copy <source> <destination>")?;
            copy_file(Path::new(source), Path::new(destination))?;
            Ok(0)
        }
        "count-copy" => {
            let counter = required(rest, 0, "count-copy <counter> <source> <destination>")?;
            let source = required(rest, 1, "count-copy <counter> <source> <destination>")?;
            let destination = required(rest, 2, "count-copy <counter> <source> <destination>")?;
            let next = read_count(Path::new(counter))? + 1;
            write_file(Path::new(counter), &format!("{next}\n"))?;
            copy_file(Path::new(source), Path::new(destination))?;
            Ok(0)
        }
        "fail-once" => {
            let marker = required(rest, 0, "fail-once <marker> <code> <text>")?;
            let code = parse_code(required(rest, 1, "fail-once <marker> <code> <text>")?)?;
            let text = required(rest, 2, "fail-once <marker> <code> <text>")?;
            if Path::new(marker).exists() {
                write_stdout(text)?;
                Ok(0)
            } else {
                write_file(Path::new(marker), "attempted")?;
                Ok(code)
            }
        }
        "write-if-exists" => {
            let check = required(rest, 0, "write-if-exists <check> <marker>")?;
            let marker = required(rest, 1, "write-if-exists <check> <marker>")?;
            if Path::new(check).exists() {
                write_file(Path::new(marker), "ran")?;
                Ok(0)
            } else {
                Ok(1)
            }
        }
        "spawn-detached" => {
            let millis = required(rest, 0, "spawn-detached <millis> <marker>")?;
            let marker = required(rest, 1, "spawn-detached <millis> <marker>")?;
            spawn_detached(millis, marker)?;
            Ok(0)
        }
        "timeout-tree" => {
            let millis = required(rest, 0, "timeout-tree <millis> <marker>")?;
            let marker = required(rest, 1, "timeout-tree <millis> <marker>")?;
            spawn_detached(millis, marker)?;
            // Stay alive so only the runner's timeout ends the task, which is
            // what forces the whole tree to be terminated.
            std::thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
        "sleep-write" => {
            let millis = required(rest, 0, "sleep-write <millis> <marker>")?;
            let marker = required(rest, 1, "sleep-write <millis> <marker>")?;
            sleep_millis(millis)?;
            write_file(Path::new(marker), "leaked")?;
            Ok(0)
        }
        other => Err(usage(&format!("unknown verb '{other}'"))),
    }
}

/// Spawn this same binary to write `marker` later, with no inherited stdio so
/// it never holds the runner's output pipes open.
fn spawn_detached(millis: &str, marker: &str) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    Command::new(executable)
        .args(["sleep-write", millis, marker])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn required<'a>(args: &'a [String], index: usize, usage: &str) -> io::Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| usage_error(usage))
}

fn parse_code(value: &str) -> io::Result<i32> {
    value
        .parse()
        .map_err(|_| usage_error(&format!("'{value}' is not an exit code")))
}

fn sleep_millis(value: &str) -> io::Result<()> {
    let millis = value
        .parse()
        .map_err(|_| usage_error(&format!("'{value}' is not a millisecond count")))?;
    std::thread::sleep(Duration::from_millis(millis));
    Ok(())
}

fn write_file(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)
}

fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination).map(|_| ())
}

fn read_count(path: &Path) -> io::Result<u64> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents
            .trim()
            .parse()
            .expect("counter file holds a number")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn write_stdout(text: &str) -> io::Result<()> {
    io::stdout().write_all(text.as_bytes())
}

fn write_stderr(text: &str) -> io::Result<()> {
    io::stderr().write_all(text.as_bytes())
}

fn usage(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn usage_error(usage_line: &str) -> io::Error {
    usage(&format!("usage: mono-fixture {usage_line}"))
}
