//! What monitrs does when whatever was reading its stdout goes away.
//!
//! `monitrs snapshot --format json | head -3` is an ordinary thing to type, and
//! until 1.0.1 it printed `monitrs: Broken pipe (os error 32)` and — under the
//! `set -o pipefail` that any careful script uses — failed the pipeline. The Rust
//! runtime sets `SIGPIPE` to `SIG_IGN` at startup, so a write to a closed pipe
//! comes back as an `io::Error` rather than killing the process, and monitrs
//! reported it like any other failure.
//!
//! These tests spawn the real binary, because that is the only way to have a real
//! pipe with a reader that really goes away. Nothing else in this workspace does
//! that: every other suite drives the modules in-process.
//!
//! # Why `snapshot` and not a cheaper subcommand
//!
//! The bug only appears when the writer is still writing after the reader has
//! gone. Anything that fits in the kernel's pipe buffer is written before the
//! reader can close, so it never fails, and a test built on it would pass whether
//! or not the fix is present. Only `snapshot` is reliably larger than any pipe
//! buffer — about 800 KB on a normal machine, against a buffer of 64 KB or so.
//! `completions` (17 KB) and `manpage` (4 KB) fit, which is exactly why the bug
//! was never noticed there, and why they are unsuitable as regression tests.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

/// The reported bug: read one line and walk away, the way `head -1` does.
#[test]
fn a_reader_that_goes_away_is_not_a_failure() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_monitrs"))
        .args(["snapshot", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary under test should spawn");

    // Scoped so the read end is dropped — and the pipe closed — while monitrs
    // still has the rest of the export to write.
    {
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut reader = BufReader::new(stdout);
        let mut first = String::new();
        reader
            .read_line(&mut first)
            .expect("monitrs should produce at least one line");
        assert_eq!(
            first.trim_end(),
            "{",
            "the export should still be the JSON document it always was"
        );
    }

    let finished = child.wait_with_output().expect("monitrs should exit");
    let complaint = String::from_utf8_lossy(&finished.stderr);

    assert!(
        complaint.is_empty(),
        "a closed pipe is the reader's decision, not monitrs' error, so nothing \
         belongs on stderr — got: {complaint}"
    );
    assert!(
        finished.status.success(),
        "exit status should be success so that `set -o pipefail` does not turn \
         `monitrs snapshot | head` into a failed pipeline — got: {:?}",
        finished.status
    );
}

/// The other half of the promise: a pipe that stays open must still deliver the
/// whole export and exit cleanly. Without this, "succeed on a closed pipe" could
/// be satisfied by never writing anything at all.
#[test]
fn a_reader_that_stays_gets_the_whole_export() {
    let finished = Command::new(env!("CARGO_BIN_EXE_monitrs"))
        .args(["snapshot", "--format", "json"])
        .output()
        .expect("monitrs should run to completion");

    assert!(finished.status.success(), "status: {:?}", finished.status);
    assert!(
        String::from_utf8_lossy(&finished.stderr).is_empty(),
        "a successful snapshot should say nothing on stderr"
    );

    let json = String::from_utf8(finished.stdout).expect("the export should be UTF-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&json).expect("the export should be complete, parseable JSON");
    assert_eq!(
        parsed["tool"]["name"], "monitrs",
        "and it should be monitrs' own export"
    );
    assert!(
        json.len() > 64 * 1024,
        "the premise of the test above is that this export cannot fit in a pipe \
         buffer; if it ever shrinks below one, that test stops discriminating — \
         got {} bytes",
        json.len()
    );
}
