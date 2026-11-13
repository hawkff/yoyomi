//! RAR archives through rars. Entries of a plain archive decode on their own.
//! Solid entries, and every entry of a split archive, come from one
//! sequential pass: rars pushes the whole archive through writers in order,
//! so a background thread runs the pass and pauses it inside those writers
//! whenever no request wants what comes next. A pass keeps its place between
//! requests and starts over only for an entry at or before its position.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use parking_lot::{Condvar, Mutex};
use rars::{ArchiveReadOptions, ErrorKind, ReadCancellation};
use yoyomi_core::{Cancel, Error, MAX_PAGE_BYTES, Result};

use super::{CANCEL_POLL, Entry, lock_or_cancel};

pub(super) struct Rar {
    volumes: Arc<[rars::Archive]>,
    /// The position of each member, by archive index, among the members a
    /// pass hands to its writers (RAR5 links get none).
    orders: Vec<usize>,
    /// Whether entries come from a pass rather than on their own.
    sequential: bool,
    /// Bytes one pass may decode. Pages barely compress, so a stream that
    /// decodes to a hundred times the size of its volumes, beyond 1 GiB, is
    /// junk built to burn CPU. Volumes count by their size on disk, not by
    /// the headers, which the archive could inflate.
    budget: u64,
    /// Cancels every pass and plain read when the book closes.
    closing: ReadCancellation,
    /// Serializes requests for the pass.
    requests: Mutex<()>,
    /// The current pass. Held only briefly, so `close` never waits on it.
    pass: Mutex<Option<Arc<Pass>>>,
}

impl Rar {
    /// Opens a RAR archive and the volumes that follow it, or returns `None`
    /// when `path` holds no RAR signature.
    pub(super) fn open(path: &Path) -> Result<Option<(Rar, Vec<Entry>)>> {
        let first = match rars::ArchiveReader::read_path(path) {
            Err(error) if matches!(error.root_cause(), rars::Error::UnsupportedSignature) => return Ok(None),
            result => result.map_err(rar_error)?,
        };
        let mut size = fs::metadata(path)?.len();
        let new_numbering = first.as_rar50().is_some() || first.as_rar15_40().is_some_and(|a| a.main.uses_new_numbering());
        let mut volumes = vec![first];
        let mut last = path.to_path_buf();
        while volumes.last().is_some_and(is_volume)
            && let Some(next) = next_volume(&last, new_numbering).filter(|next| next.is_file())
        {
            size += fs::metadata(&next)?.len();
            volumes.push(rars::ArchiveReader::read_path(&next).map_err(rar_error)?);
            last = next;
        }
        let members = rars::volume_members(&volumes).map_err(rar_error)?;
        let mut orders = Vec::with_capacity(members.len());
        let mut entries = Vec::new();
        let mut order = 0;
        for (index, member) in members.iter().enumerate() {
            orders.push(order);
            if !member.meta.is_redirection {
                order += 1;
            }
            if !member.meta.is_directory && !member.meta.is_redirection {
                entries.push(Entry { name: entry_name(&member.meta.name_lossy()), index });
            }
        }
        let sequential = volumes.len() > 1 || volumes[0].is_solid();
        let rar = Rar {
            volumes: volumes.into(),
            orders,
            sequential,
            budget: (1 << 30).max(size.saturating_mul(100)),
            closing: ReadCancellation::new(),
            requests: Mutex::new(()),
            pass: Mutex::new(None),
        };
        Ok(Some((rar, entries)))
    }

    /// The bytes of entry `index`, at most `MAX_PAGE_BYTES` of them.
    pub(super) fn read(&self, index: usize, name: &str, cancel: &Cancel) -> Result<Vec<u8>> {
        if !self.sequential {
            return self.read_plain(index, cancel);
        }
        let _request = lock_or_cancel(&self.requests, cancel)?;
        if self.closing.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let order = self.orders[index];
        let pass = {
            let mut slot = self.pass.lock();
            let current = slot.as_ref().filter(|pass| pass.reaches(order)).cloned();
            match current {
                Some(pass) => pass,
                None => {
                    if let Some(old) = slot.take() {
                        old.halt();
                    }
                    let pass = Pass::start(self.volumes.clone(), self.budget, self.closing.child())?;
                    *slot = Some(pass.clone());
                    pass
                }
            }
        };
        pass.read(order, name, cancel)
    }

    fn read_plain(&self, index: usize, cancel: &Cancel) -> Result<Vec<u8>> {
        cancel.check()?;
        let options = ArchiveReadOptions::new().with_max_member_output_bytes(MAX_PAGE_BYTES as u64).with_cancellation(&self.closing);
        // shortcut: rars decodes a member whole, so a plain RAR read cannot
        // stop inside one; the request is checked before and after. A page
        // decodes in milliseconds; bridge `cancel` to `closing` if not.
        let data = self.volumes[0].read_member_at_with_options(index, options).map_err(rar_error)?;
        cancel.check()?;
        data.ok_or_else(|| Error::Format(format!("RAR entry {index} is missing")))
    }

    /// Stops every decode without waiting for one.
    pub(super) fn close(&self) {
        self.closing.cancel();
        if let Some(pass) = self.pass.lock().take() {
            pass.halt();
        }
    }
}

impl Drop for Rar {
    fn drop(&mut self) {
        self.close();
    }
}

/// One sequential decode of the whole archive, run by its own thread.
struct Pass {
    state: Mutex<PassState>,
    /// Wakes the thread and the waiting request on every change.
    wake: Condvar,
    stop: ReadCancellation,
}

#[derive(Default)]
struct PassState {
    /// The member a request waits for, by order, and that request's token.
    want: Option<(usize, Cancel)>,
    /// How many members the pass has started; it is decoding `next - 1`.
    next: usize,
    /// The name of the member being decoded.
    name: String,
    /// Its bytes so far; `None` once a request took them.
    data: Option<Vec<u8>>,
    /// It holds more than `MAX_PAGE_BYTES`.
    overflow: bool,
    /// It is complete and verified.
    finished: bool,
    ended: bool,
    error: Option<Error>,
}

impl PassState {
    /// Whether a live request wants `member` or a later one.
    fn wants(&self, member: usize) -> bool {
        self.want.as_ref().is_some_and(|(want, cancel)| *want >= member && !cancel.is_cancelled())
    }
}

impl Pass {
    fn start(volumes: Arc<[rars::Archive]>, budget: u64, stop: ReadCancellation) -> Result<Arc<Pass>> {
        let pass = Arc::new(Pass { state: Mutex::default(), wake: Condvar::new(), stop });
        let runner = pass.clone();
        thread::Builder::new().name("rar-pass".into()).spawn(move || runner.run(&volumes, budget))?;
        Ok(pass)
    }

    fn run(self: Arc<Self>, volumes: &[rars::Archive], budget: u64) {
        let options = ArchiveReadOptions::new().with_max_total_output_bytes(budget).with_cancellation(&self.stop);
        let result = rars::extract_volumes_to_with_options(volumes, options, |meta| {
            self.begin(entry_name(&meta.name_lossy()))?;
            Ok(Box::new(PassWriter(self.clone())) as Box<dyn Write>)
        });
        let mut state = self.state.lock();
        state.ended = true;
        match result {
            Ok(()) => state.finished = true,
            Err(error) => state.error = Some(rar_error(error)),
        }
        self.wake.notify_all();
    }

    /// Finishes the member being decoded and starts the next one once a
    /// request wants it.
    fn begin(&self, name: String) -> rars::Result<()> {
        let mut state = self.state.lock();
        state.finished = true;
        self.wake.notify_all();
        while !state.wants(state.next) {
            if self.stop.is_cancelled() {
                return Err(rars::Error::Cancelled);
            }
            self.wake.wait(&mut state);
        }
        state.next += 1;
        state.name = name;
        state.data = Some(Vec::new());
        state.overflow = false;
        state.finished = false;
        Ok(())
    }

    /// Whether the pass can still hand over member `order`.
    fn reaches(&self, order: usize) -> bool {
        let state = self.state.lock();
        state.error.is_none() && if order + 1 == state.next { state.data.is_some() } else { order >= state.next && !state.ended }
    }

    fn read(&self, order: usize, name: &str, cancel: &Cancel) -> Result<Vec<u8>> {
        let mut state = self.state.lock();
        state.want = Some((order, cancel.clone()));
        self.wake.notify_all();
        let result = loop {
            if state.finished
                && state.next == order + 1
                && let Some(data) = state.data.take()
            {
                break if state.name != name {
                    Err(Error::Format(format!("RAR entry {order} is {:?}, want {name:?}", state.name)))
                } else if state.overflow {
                    Err(Error::Limit(format!("{name} exceeds {MAX_PAGE_BYTES} bytes")))
                } else {
                    Ok(data)
                };
            }
            if let Some(error) = &state.error {
                break Err(error.clone());
            }
            if state.ended {
                break Err(Error::Format(format!("RAR entry {name} is missing")));
            }
            if cancel.is_cancelled() || self.stop.is_cancelled() {
                break Err(Error::Cancelled);
            }
            self.wake.wait_for(&mut state, CANCEL_POLL);
        };
        state.want = None;
        result
    }

    /// Stops the thread at its next write without waiting for it.
    fn halt(&self) {
        self.stop.cancel();
        let _state = self.state.lock();
        self.wake.notify_all();
    }
}

/// Collects the member being decoded, and pauses the pass while no request
/// wants it.
struct PassWriter(Arc<Pass>);

impl Write for PassWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let pass = &self.0;
        let mut guard = pass.state.lock();
        while !guard.wants(guard.next - 1) {
            if pass.stop.is_cancelled() {
                // Not `Interrupted`, which `write_all` would retry forever.
                return Err(io::Error::other("RAR pass stopped"));
            }
            pass.wake.wait(&mut guard);
        }
        let state = &mut *guard;
        match &mut state.data {
            Some(data) if !state.overflow && data.len() + buf.len() <= MAX_PAGE_BYTES => data.extend_from_slice(buf),
            _ => state.overflow = true,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn rar_error(error: rars::Error) -> Error {
    match error.root_cause() {
        rars::Error::TotalOutputLimitExceeded { limit, .. } => Error::Limit(format!("RAR archive expands past {} MiB", limit >> 20)),
        _ if error.kind() == ErrorKind::Cancelled => Error::Cancelled,
        _ if error.kind() == ErrorKind::ResourceLimit => Error::Limit(error.to_string()),
        _ => Error::Format(error.to_string()),
    }
}

/// Entry names use `/`, as RAR archives from Windows may not.
fn entry_name(name: &str) -> String {
    name.replace('\\', "/")
}

fn is_volume(archive: &rars::Archive) -> bool {
    archive.as_rar13().is_some_and(|a| a.main.is_volume())
        || archive.as_rar15_40().is_some_and(|a| a.main.is_volume())
        || archive.as_rar50().is_some_and(|a| a.main.is_volume())
}

/// The volume after `path`: in the new numbering `name.part1.rar` is
/// followed by `name.part2.rar`, and in the old one `name.rar` by `name.r00`,
/// then `name.r01`, with `name.r99` carrying into `name.s00`.
fn next_volume(path: &Path, new_numbering: bool) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let (stem, ext) = name.rsplit_once('.')?;
    let next = if new_numbering {
        let digits = stem.len() - stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        format!("{}.{ext}", increment(stem, digits)?)
    } else if let &[letter, tens @ b'0'..=b'9', ones @ b'0'..=b'9'] = ext.as_bytes()
        && letter.is_ascii_alphabetic()
    {
        let number = (tens - b'0') * 10 + (ones - b'0') + 1;
        let (letter, number) = if number == 100 { (letter + 1, 0) } else { (letter, number) };
        if !letter.is_ascii_alphabetic() {
            return None;
        }
        format!("{stem}.{}{number:02}", char::from(letter))
    } else {
        format!("{stem}.r00")
    };
    Some(path.with_file_name(next))
}

/// Adds one to the number in the last `width` bytes of `s`, keeping its width.
fn increment(s: &str, width: usize) -> Option<String> {
    if width == 0 {
        return None;
    }
    let (head, number) = s.split_at(s.len() - width);
    let number = number.parse::<u64>().ok()? + 1;
    Some(format!("{head}{number:0width$}"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::super::tests::assert_cancel_unblocks;
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
    }

    fn open(name: &str) -> (Rar, Vec<Entry>) {
        Rar::open(&fixture(name)).unwrap().unwrap()
    }

    fn read(rar: &Rar, entry: &Entry) -> Result<Vec<u8>> {
        rar.read(entry.index, &entry.name, &Cancel::new())
    }

    fn current_pass(rar: &Rar) -> Arc<Pass> {
        rar.pass.lock().clone().unwrap()
    }

    /// Every entry's bytes, read in order by a separate archive.
    fn reference(name: &str) -> Vec<Vec<u8>> {
        let (rar, entries) = open(name);
        entries.iter().map(|entry| read(&rar, entry).unwrap()).collect()
    }

    #[test]
    fn solid_fixtures_have_solid_entries() {
        for name in ["solid.cbr", "solid-bmp.cbr"] {
            let (rar, entries) = open(name);
            let members = rars::volume_members(&rar.volumes).unwrap();
            assert_eq!(entries.len(), 3, "{name}");
            assert!(rar.sequential && members[entries[1].index].is_solid(), "{name} needs solid files after the first");
        }
    }

    #[test]
    fn a_pass_continues_forward_and_starts_over_backward() {
        let (rar, entries) = open("solid-bmp.cbr");
        let want = reference("solid-bmp.cbr");
        assert_eq!(read(&rar, &entries[1]).unwrap(), want[1]);
        let pass = current_pass(&rar);
        // A cancelled request leaves the pass where it was.
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(rar.read(entries[2].index, &entries[2].name, &cancel).unwrap_err().is_cancelled());
        assert_eq!(read(&rar, &entries[2]).unwrap(), want[2]);
        assert!(Arc::ptr_eq(&pass, &current_pass(&rar)), "a later page continues the pass");
        assert_eq!(read(&rar, &entries[0]).unwrap(), want[0]);
        assert!(!Arc::ptr_eq(&pass, &current_pass(&rar)), "an earlier page starts over");
    }

    #[test]
    fn requests_cancelled_mid_decode_leave_the_pass_usable() {
        let (rar, entries) = open("solid-bmp.cbr");
        let want = reference("solid-bmp.cbr");
        for delay in [0, 1, 2, 5] {
            let cancel = Cancel::new();
            let stopper = cancel.clone();
            let stop = thread::spawn(move || {
                thread::sleep(Duration::from_micros(delay));
                stopper.cancel();
            });
            match rar.read(entries[2].index, &entries[2].name, &cancel) {
                Ok(data) => assert_eq!(data, want[2]),
                Err(error) => assert!(error.is_cancelled(), "{error}"),
            }
            stop.join().unwrap();
            for i in [1, 2] {
                assert_eq!(read(&rar, &entries[i]).unwrap(), want[i], "page {i} after a stop at {delay} µs");
            }
        }
    }

    #[test]
    fn a_pass_stops_at_its_budget() {
        let (mut rar, entries) = open("solid.cbr");
        let size: usize = entries.iter().map(|entry| read(&rar, entry).unwrap().len()).sum();
        // One pass may decode every file before a page until the budget runs out.
        rar.budget = size as u64;
        assert!(read(&rar, &entries[2]).is_ok());
        rar.budget = size as u64 - 1;
        let error = read(&rar, &entries[2]).unwrap_err();
        assert!(error.to_string().contains("expands past"), "{error}");
    }

    #[test]
    fn close_does_not_wait_for_the_decoder() {
        let (rar, entries) = open("solid.cbr");
        read(&rar, &entries[1]).unwrap();
        let pass = current_pass(&rar);
        let rar = Arc::new(rar);
        let request = rar.requests.lock(); // a request in the middle of a decode
        let closer = rar.clone();
        let closed = thread::spawn(move || closer.close());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !closed.is_finished() {
            assert!(Instant::now() < deadline, "close waited for the decoder");
            thread::sleep(Duration::from_millis(1));
        }
        drop(request);
        while !pass.state.lock().ended {
            assert!(Instant::now() < deadline, "the pass kept running after close");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(read(&rar, &entries[2]).unwrap_err().is_cancelled());
    }

    #[test]
    fn split_volumes_open_from_the_first_part() {
        let (rar, entries) = open("split.part1.rar");
        assert_eq!(rar.volumes.len(), 3);
        assert!(rar.sequential);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["a.bin", "b.bin", "c.bin"]);
        for entry in &entries {
            assert!(!read(&rar, entry).unwrap().is_empty(), "{}", entry.name);
        }
        let size: u64 = (1..=3).map(|n| fs::metadata(fixture(&format!("split.part{n}.rar"))).unwrap().len()).sum();
        assert_eq!(rar.budget, (1 << 30).max(100 * size));
    }

    #[test]
    fn volume_names_count_up() {
        let next = |name: &str, new| next_volume(Path::new(name), new).unwrap().to_str().unwrap().to_owned();
        assert_eq!(next("dir/split.part1.rar", true), "dir/split.part2.rar");
        assert_eq!(next("book.part09.cbr", true), "book.part10.cbr");
        assert_eq!(next("Batman 01.rar", false), "Batman 01.r00");
        assert_eq!(next("book.r09", false), "book.r10");
        assert_eq!(next("book.r99", false), "book.s00");
        assert_eq!(next("BOOK.S41", false), "BOOK.S42");
        assert_eq!(next("book.s99", false), "book.t00");
        assert_eq!(next_volume(Path::new("book.z99"), false), None);
        assert_eq!(next_volume(Path::new("book.rar"), true), None);
    }

    /// A RAR 1.5 volume holding one stored file, its main header without the
    /// new numbering flag.
    fn old_volume(first: bool, name: &str, data: &[u8]) -> Vec<u8> {
        let header_crc = |header: &mut [u8]| {
            let crc = rars::crc32::crc32(&header[2..]) as u16;
            header[..2].copy_from_slice(&crc.to_le_bytes());
        };
        let mut main = vec![0, 0, 0x73];
        main.extend_from_slice(&(if first { 0x0101u16 } else { 0x0001 }).to_le_bytes()); // volume, first volume
        main.extend_from_slice(&13u16.to_le_bytes());
        main.extend_from_slice(&[0; 6]);
        header_crc(&mut main);
        let mut file = vec![0, 0, 0x74];
        file.extend_from_slice(&0x8000u16.to_le_bytes()); // long block
        file.extend_from_slice(&(32 + name.len() as u16).to_le_bytes());
        file.extend_from_slice(&(data.len() as u32).to_le_bytes()); // packed
        file.extend_from_slice(&(data.len() as u32).to_le_bytes()); // unpacked
        file.push(0); // DOS
        file.extend_from_slice(&rars::crc32::crc32(data).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes()); // time
        file.extend_from_slice(&[15, 0x30]); // version 1.5, stored
        file.extend_from_slice(&(name.len() as u16).to_le_bytes());
        file.extend_from_slice(&0x20u32.to_le_bytes()); // archive attribute
        file.extend_from_slice(name.as_bytes());
        header_crc(&mut file);
        [&b"Rar!\x1a\x07\x00"[..], &main, &file, data].concat()
    }

    #[test]
    fn old_numbered_volumes_carry_into_the_next_letter() {
        let dir = tempfile::tempdir().unwrap();
        for (i, (volume, entry)) in [("book.r99", "a.bin"), ("book.s00", "b.bin"), ("book.s01", "c.bin")].into_iter().enumerate() {
            fs::write(dir.path().join(volume), old_volume(i == 0, entry, entry.as_bytes())).unwrap();
        }
        let (rar, entries) = Rar::open(&dir.path().join("book.r99")).unwrap().unwrap();
        assert_eq!(rar.volumes.len(), 3);
        for (entry, name) in entries.iter().zip(["a.bin", "b.bin", "c.bin"]) {
            assert_eq!(entry.name, name);
            assert_eq!(read(&rar, entry).unwrap(), name.as_bytes());
        }
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn cancelled_requests_do_not_wait_for_another_decode() {
        let (rar, entries) = open("solid.cbr");
        let rar = Arc::new(rar);
        let decode = rar.requests.lock(); // another page in the middle of a slow decode
        let (reader, entry) = (rar.clone(), entries[1].clone());
        assert_cancel_unblocks(move |cancel| reader.read(entry.index, &entry.name, cancel));
        drop(decode);
        assert!(read(&rar, &entries[1]).is_ok());
    }

    #[test]
    fn other_files_are_not_rar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "plain text").unwrap();
        assert!(Rar::open(&path).unwrap().is_none());
    }
}
