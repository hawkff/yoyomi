//! `yy -c` on every fixture in `testdata/`.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn check_reports_every_fixture() {
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
    let antiword = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir| dir.join("antiword").is_file());
    if !antiword {
        eprintln!("skipped: antiword is not installed");
    }
    let expected = [
        ("nested.cbz", "OK (5 pages;"),
        ("paragraphs.doc", if antiword { "OK (1 pages;" } else { "antiword is not on PATH" }),
        ("solid-bmp.cbr", "OK (3 pages;"),
        ("solid.cbr", "OK (3 pages;"),
        // The split volumes test volume chaining and hold no images.
        ("split.part1.rar", "no images found"),
        ("split.part2.rar", "volume set starts with a continuation"),
        ("split.part3.rar", "volume set starts with a continuation"),
        ("webtoon.cbz", "OK (4 pages;"),
    ];
    let mut fixtures: Vec<String> = testdata
        .read_dir()
        .unwrap()
        .filter(|entry| entry.as_ref().is_ok_and(|entry| entry.file_type().is_ok_and(|kind| kind.is_file())))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.ends_with(".py"))
        .collect();
    fixtures.sort();
    assert_eq!(fixtures, expected.map(|(name, _)| name), "a fixture lacks an expected report");

    let mut yy = Command::new(env!("CARGO_BIN_EXE_yy"))
        .arg("-c")
        .args(expected.map(|(name, _)| testdata.join(name)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let read = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| String::from_utf8_lossy(&bytes).into_owned())
        })
    };
    let stdout = read(Box::new(yy.stdout.take().unwrap()));
    let stderr = read(Box::new(yy.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = yy.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            // A hung yy must not outlive the test.
            yy.kill().unwrap();
            yy.wait().unwrap();
            panic!("yy -c did not finish within 60 seconds");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1), "the split fixtures fail");
    let report = stdout.join().unwrap().unwrap() + &stderr.join().unwrap().unwrap();
    for (name, said) in expected {
        assert!(report.lines().any(|line| line.contains(name) && line.contains(said)), "{name}: want {said:?} in {report}");
    }
}
