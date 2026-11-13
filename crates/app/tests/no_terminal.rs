//! `yy book` without a controlling terminal.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn no_terminal_fails_before_writing_to_stdout() {
    let book = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/nested.cbz");
    // setsid runs yy in a new session, which has no controlling terminal.
    let out = match Command::new("setsid").arg("--wait").arg(env!("CARGO_BIN_EXE_yy")).arg(book).stdin(Stdio::null()).output() {
        Ok(out) => out,
        // macOS ships no setsid binary.
        Err(e) if e.kind() == ErrorKind::NotFound => {
            eprintln!("skipped: setsid is not on PATH");
            return;
        }
        Err(e) => panic!("setsid runs yy: {e}"),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.starts_with("yy: no terminal: open /dev/tty: "), "{stderr}");
    assert!(out.stdout.is_empty(), "{:?}", String::from_utf8_lossy(&out.stdout));
}
