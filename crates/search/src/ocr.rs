//! Page text from tesseract.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use yoyomi_core::image::codecs::png::{CompressionType, FilterType, PngEncoder};
use yoyomi_core::image::{ExtendedColorType, ImageEncoder};
use yoyomi_core::{Cancel, Error, Result, RgbaImage};

/// How long tesseract may take for one page.
const OCR_TIMEOUT: Duration = Duration::from_secs(90);

/// How often a running tesseract checks its cancel token.
const POLL: Duration = Duration::from_millis(10);

/// How many characters of stderr a failure message keeps.
const MAX_MESSAGE_CHARS: usize = 120;

/// Whether an executable `tesseract` is on `PATH`.
pub fn tesseract_available() -> bool {
    env::var_os("PATH").is_some_and(|paths| env::split_paths(&paths).any(|dir| is_executable(&dir.join("tesseract"))))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The tesseract language: `YOYOMI_OCR_LANG`, or `eng`.
pub fn ocr_lang() -> String {
    env::var("YOYOMI_OCR_LANG").ok().filter(|lang| !lang.is_empty()).unwrap_or_else(|| "eng".to_owned())
}

/// Recognizes the text of `page` with tesseract in `ocr_lang()`. `cancel`
/// kills the run, as does a run past 90 seconds.
pub fn recognize(page: &RgbaImage, cancel: &Cancel) -> Result<String> {
    recognize_in(page, cancel, &Mutex::new(None))
}

/// `recognize`, with the tesseract child held in `running` while it runs, so
/// `kill_running` can stop it from another thread.
pub(crate) fn recognize_in(page: &RgbaImage, cancel: &Cancel, running: &Mutex<Option<Child>>) -> Result<String> {
    cancel.check()?;
    let tool_error = |message: String| Error::Tool { tool: "tesseract".to_owned(), message };
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive).write_image(
        page.as_raw(),
        page.width(),
        page.height(),
        ExtendedColorType::Rgba8,
    )?;
    let (mut stdin, stdout, stderr) = {
        let mut slot = running.lock().unwrap_or_else(PoisonError::into_inner);
        // Checked under the lock: a cancel followed by `kill_running` either
        // stops the spawn here or finds the child in `running`.
        cancel.check()?;
        let mut child = match Command::new("tesseract")
            .args(["stdin", "stdout", "-l", &ocr_lang()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(tool_error("not on PATH".into())),
            Err(error) => return Err(tool_error(error.to_string())),
        };
        let pipes =
            (child.stdin.take().expect("piped stdin"), child.stdout.take().expect("piped stdout"), child.stderr.take().expect("piped stderr"));
        *slot = Some(child);
        pipes
    };
    // tesseract may fail before it reads the whole image; its stderr says why.
    thread::spawn(move || stdin.write_all(&png));
    let stdout = read_in_background(stdout);
    let stderr = read_in_background(stderr);
    let deadline = Instant::now() + OCR_TIMEOUT;
    let status = loop {
        {
            let mut slot = running.lock().unwrap_or_else(PoisonError::into_inner);
            // `kill_running` took and reaped the child.
            let Some(child) = slot.as_mut() else { return Err(Error::Cancelled) };
            if let Some(status) = child.try_wait()? {
                *slot = None;
                break status;
            }
        }
        if cancel.is_cancelled() {
            kill_running(running);
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_running(running);
            return Err(tool_error(format!("gave no result within {}s", OCR_TIMEOUT.as_secs())));
        }
        thread::sleep(POLL);
    };
    let stdout = stdout.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).trim().to_owned();
        let message = if stderr.is_empty() { status.to_string() } else { stderr.chars().take(MAX_MESSAGE_CHARS).collect() };
        return Err(tool_error(message));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Kills and reaps the tesseract child in `running`, if there is one.
pub(crate) fn kill_running(running: &Mutex<Option<Child>>) {
    if let Some(mut child) = running.lock().unwrap_or_else(PoisonError::into_inner).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Drains `pipe` on its own thread, so tesseract never blocks on a full pipe.
fn read_in_background(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut out = Vec::new();
        let _ = pipe.read_to_end(&mut out);
        out
    })
}
