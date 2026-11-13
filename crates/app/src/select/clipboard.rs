//! The clipboard and the browser: OSC 52 for the terminal, local clipboard
//! tools beside it, and `xdg-open` or `open` for links.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use std::{env, thread};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// How long one clipboard tool may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// The OSC 52 sequence that sets the terminal's clipboard to `text`.
pub(crate) fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text)).into_bytes()
}

/// Which local clipboards the session can reach.
#[derive(Clone, Debug, Default)]
pub(crate) struct Session {
    tmux: bool,
    wayland: bool,
    x11: bool,
    mac: bool,
    /// Where to look for the tools instead of `PATH`.
    path: Option<OsString>,
}

impl Session {
    pub(crate) fn current() -> Self {
        let set = |name| env::var_os(name).is_some_and(|value| !value.is_empty());
        Session { tmux: set("TMUX"), wayland: set("WAYLAND_DISPLAY"), x11: set("DISPLAY"), mac: cfg!(target_os = "macos"), path: None }
    }
}

/// Copies to local clipboard tools one at a time. A copy queued behind a
/// slow tool gives way to a newer one.
pub(crate) struct Copies {
    issued: AtomicU64,
    newest: AtomicU64,
    running: Mutex<()>,
}

/// The copies of this process.
pub(crate) static COPIES: Copies = Copies::new();

impl Copies {
    const fn new() -> Self {
        Copies { issued: AtomicU64::new(0), newest: AtomicU64::new(0), running: Mutex::new(()) }
    }

    /// Numbers a new copy; later copies get larger numbers.
    pub(crate) fn issue(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Hands `text`, copy number `copy`, to local clipboard tools: tmux drops
    /// OSC 52 under its default `set-clipboard`, and macOS Terminal and VTE
    /// terminals such as GNOME Terminal ignore it. Inside tmux its buffer
    /// gets the text, then the first of pbcopy, wl-copy and xclip that works.
    /// Missing tools fail silently.
    pub(crate) fn copy(&self, copy: u64, text: &str, session: &Session) {
        self.newest.fetch_max(copy, Ordering::SeqCst);
        let _running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if copy < self.newest.load(Ordering::SeqCst) {
            return;
        }
        if session.tmux {
            let _ = run(session, &["tmux", "load-buffer", "-w", "-"], text);
        }
        let tools: [(bool, &[&str]); 3] =
            [(session.mac, &["pbcopy"]), (session.wayland, &["wl-copy"]), (session.x11, &["xclip", "-selection", "clipboard"])];
        // xclip also reaches Xwayland when wl-copy is missing.
        for (_, tool) in tools.iter().filter(|(usable, _)| *usable) {
            if run(session, tool, text).is_ok() {
                return;
            }
        }
    }
}

/// Runs `tool` with `text` on its input, within `TOOL_TIMEOUT`.
fn run(session: &Session, tool: &[&str], text: &str) -> io::Result<()> {
    let mut command = Command::new(tool[0]);
    command.args(&tool[1..]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(path) = &session.path {
        command.env("PATH", path);
    }
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let text = text.to_owned();
        // A tool that does not read its input must not hold up the timeout.
        thread::spawn(move || stdin.write_all(text.as_bytes()));
    }
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() { Ok(()) } else { Err(io::Error::other(format!("{}: {status}", tool[0]))) };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("{} timed out", tool[0])));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Opens a web or mail link in the desktop's browser or mail program.
pub(crate) fn open_url(url: &str) -> io::Result<()> {
    let web = url.split_once(':').is_some_and(|(scheme, _)| ["http", "https", "mailto"].contains(&scheme.to_ascii_lowercase().as_str()));
    if !web {
        return Err(io::Error::other("not a web or mail link"));
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut child = Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    fn script(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn osc52_carries_base64() {
        assert_eq!(osc52("copied text"), b"\x1b]52;c;Y29waWVkIHRleHQ=\x07");
    }

    #[test]
    fn copies_reach_local_clipboard_tools() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let record = format!("printf '%s %s:' \"$(basename \"$0\")\" \"$*\" >> '{0}'\ncat >> '{0}'\nprintf '\\n' >> '{0}'\n", log.display());
        for tool in ["tmux", "pbcopy", "wl-copy", "xclip"] {
            script(dir.path(), tool, &record);
        }
        let mut path = OsString::from(dir.path());
        path.push(":");
        path.push(env::var_os("PATH").unwrap_or_default());
        let read = || fs::read_to_string(&log).unwrap_or_default();
        let copies = Copies::new();
        let mut session = Session { tmux: true, wayland: true, x11: true, mac: false, path: Some(path) };

        copies.copy(1, "copied text", &session);
        assert_eq!(read(), "tmux load-buffer -w -:copied text\nwl-copy :copied text\n");
        session.mac = true;
        fs::remove_file(&log).unwrap();
        copies.copy(2, "mac", &session);
        assert_eq!(read(), "tmux load-buffer -w -:mac\npbcopy :mac\n");
        session.mac = false;

        // A failing wl-copy falls back to xclip through Xwayland.
        script(dir.path(), "wl-copy", "exit 1\n");
        fs::remove_file(&log).unwrap();
        copies.copy(3, "fallback", &session);
        assert_eq!(read(), "tmux load-buffer -w -:fallback\nxclip -selection clipboard:fallback\n");

        // Without tmux or a display, only the terminal's OSC 52 copy remains.
        fs::remove_file(&log).unwrap();
        let bare = Session { path: session.path.clone(), ..Session::default() };
        copies.copy(4, "ignored", &bare);
        assert!(!log.exists(), "a session without a display ran a clipboard tool");

        // A copy that starts late cannot replace a newer one.
        copies.copy(3, "stale", &session);
        assert!(!log.exists(), "an older copy ran after a newer one");
    }

    #[test]
    fn copy_numbers_grow() {
        let copies = Copies::new();
        assert!(copies.issue() < copies.issue());
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(open_url("file:///etc/passwd").is_err());
        assert!(open_url("-x").is_err());
    }
}
