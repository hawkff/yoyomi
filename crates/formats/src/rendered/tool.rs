//! Runs poppler and djvulibre tools with bounded output, a time limit and
//! cancellation.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use yoyomi_core::{Cancel, Error, Result};

/// How long one tool run may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// How much of a tool's stderr an error may quote from.
const MAX_DIAGNOSTICS: u64 = 4 << 10;

/// How many characters of stderr a failure message keeps.
const MAX_MESSAGE_CHARS: usize = 200;

/// How often a running tool, or a request waiting for one, checks its stop
/// tokens.
pub(super) const POLL: Duration = Duration::from_millis(5);

/// Runs `command` and returns its stdout, which may not exceed `limit` bytes.
/// Any of `stops` kills the tool, as does `TOOL_TIMEOUT`.
pub(super) fn run(command: &mut Command, limit: usize, stops: &[&Cancel]) -> Result<Vec<u8>> {
    run_within(command, limit, TOOL_TIMEOUT, stops)
}

fn run_within(command: &mut Command, limit: usize, timeout: Duration, stops: &[&Cancel]) -> Result<Vec<u8>> {
    let name = command.get_program().to_string_lossy().into_owned();
    let tool_error = |message: String| Error::Tool { tool: name.clone(), message };
    stops.iter().try_for_each(|stop| stop.check())?;
    let mut child = match command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(tool_error("not on PATH".into())),
        Err(error) => return Err(tool_error(error.to_string())),
    };
    let (mut stdout, stderr) = (child.stdout.take().expect("piped stdout"), child.stderr.take().expect("piped stderr"));
    let diagnostics = thread::spawn(move || read_diagnostics(stderr));
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut out = Vec::new();
        let _ = sender.send(stdout.by_ref().take(limit as u64 + 1).read_to_end(&mut out).map(|_| out));
    });
    let deadline = Instant::now() + timeout;
    let mut output = None;
    let status = loop {
        if stops.iter().any(|stop| stop.is_cancelled()) {
            stop(&mut child);
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(tool_error(timeout_message(timeout)));
        }
        if output.is_none() {
            match receiver.recv_timeout(POLL) {
                Ok(Ok(out)) if out.len() > limit => {
                    stop(&mut child);
                    return Err(tool_error(format!("output exceeds {limit} bytes")));
                }
                Ok(Ok(out)) => output = Some(out),
                Ok(Err(error)) => {
                    stop(&mut child);
                    return Err(tool_error(error.to_string()));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    stop(&mut child);
                    return Err(tool_error("stdout reader stopped".into()));
                }
            }
        } else if let Some(status) = child.try_wait()? {
            break status;
        } else {
            thread::sleep(POLL);
        }
    };
    let diagnostics = diagnostics.join().unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&diagnostics);
        let text = text.trim();
        let message = if text.is_empty() { status.to_string() } else { text.chars().take(MAX_MESSAGE_CHARS).collect() };
        return Err(tool_error(message));
    }
    Ok(output.unwrap_or_default())
}

/// Says how long a tool had, in minutes and seconds.
fn timeout_message(timeout: Duration) -> String {
    format!("gave no result within {}m{}s", timeout.as_secs() / 60, timeout.as_secs() % 60)
}

/// Kills a tool and reaps it. The reader threads end once its pipes close.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Keeps the first `MAX_DIAGNOSTICS` bytes of `stderr` and drains the rest,
/// so a noisy tool never blocks on a full pipe.
fn read_diagnostics(mut stderr: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = stderr.by_ref().take(MAX_DIAGNOSTICS).read_to_end(&mut kept);
    let _ = io::copy(&mut stderr, &mut io::sink());
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn tool_message(error: Error) -> (String, String) {
        match error {
            Error::Tool { tool, message } => (tool, message),
            other => panic!("not a tool error: {other:?}"),
        }
    }

    #[test]
    fn diagnostics_keep_a_bounded_prefix() {
        let mut payload = vec![b'x'; 3 * MAX_DIAGNOSTICS as usize];
        payload.extend(vec![b'y'; 3 * MAX_DIAGNOSTICS as usize]);
        assert_eq!(read_diagnostics(payload.as_slice()), &payload[..MAX_DIAGNOSTICS as usize]);
    }

    #[test]
    fn a_noisy_tool_still_succeeds() {
        let out = run(&mut sh("head -c 1048576 /dev/zero | tr '\\0' x >&2; printf ok"), 16, &[]).unwrap();
        assert_eq!(out, b"ok");
    }

    #[test]
    fn a_failure_quotes_the_start_of_stderr() {
        let error = run(&mut sh("head -c 1048576 /dev/zero | tr '\\0' x >&2; exit 1"), 16, &[]).unwrap_err();
        assert_eq!(error.to_string(), format!("sh: {}", "x".repeat(MAX_MESSAGE_CHARS)));
    }

    #[test]
    fn a_silent_failure_reports_the_exit_status() {
        assert_eq!(tool_message(run(&mut sh("exit 3"), 16, &[]).unwrap_err()), ("sh".into(), "exit status: 3".into()));
    }

    #[test]
    fn a_missing_tool_is_not_on_path() {
        let error = run(&mut Command::new("yoyomi-no-such-tool"), 16, &[]).unwrap_err();
        assert_eq!(error.to_string(), "yoyomi-no-such-tool: not on PATH");
    }

    #[test]
    fn output_past_the_limit_fails() {
        let error = run(&mut sh("head -c 1048576 /dev/zero"), 1000, &[]).unwrap_err();
        assert_eq!(error.to_string(), "sh: output exceeds 1000 bytes");
        assert_eq!(run(&mut sh("printf 1234"), 4, &[]).unwrap(), b"1234");
    }

    #[test]
    fn a_stopped_tool_reports_why() {
        let started = Instant::now();
        let error = run_within(Command::new("sleep").arg("10"), 16, Duration::from_millis(50), &[]).unwrap_err();
        // The exit status of a killed tool says only "signal: 9".
        assert_eq!(error.to_string(), "sleep: gave no result within 0m0s");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(timeout_message(TOOL_TIMEOUT), "gave no result within 2m0s");
    }

    #[test]
    fn any_stop_token_kills_a_running_tool() {
        for which in 0..2 {
            let (request, closed) = (Cancel::new(), Cancel::new());
            let token = [&request, &closed][which].clone();
            let stopper = thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                token.cancel();
            });
            let started = Instant::now();
            let result = run(Command::new("sleep").arg("10"), 16, &[&request, &closed]);
            assert!(result.unwrap_err().is_cancelled());
            assert!(started.elapsed() < Duration::from_secs(5));
            stopper.join().unwrap();
        }
    }

    #[test]
    fn a_cancelled_token_never_starts_the_tool() {
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(run(&mut Command::new("yoyomi-no-such-tool"), 16, &[&cancel]).unwrap_err().is_cancelled());
    }
}
