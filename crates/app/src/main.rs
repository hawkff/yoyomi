//! `yy` reads comics and books in the terminal.
//!
//! ```text
//! yy one.cbz            read one book
//! yy one.cbz two.cbr    split screen, two books side by side
//! yy                    start with an empty pane; press o to open
//! yy -c book ...        check books and exit
//! ```

mod app;
mod draw;
#[cfg(test)]
mod end_to_end;
mod menus;
mod picker;
mod search;
mod select;
mod shot;

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, panic, thread};

use crossbeam_channel::{Receiver, Sender, at, never, select, unbounded};
use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event as Input, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use yoyomi_core::{CellSize, ImageIds, Renderer, RendererKind};
use yoyomi_formats::Opened;
use yoyomi_render::{Terminal, cell_size, detect, needs_probe, probe};
use yoyomi_state::{Bookmarks, Positions};
use yoyomi_view::{Resident, ViewEvent};

use crate::app::{App, Event};

const USAGE: &str = "\
yy reads comics and books in the terminal.

usage:
  yy [flags] [book [book2]]   one book, or two side by side
  yy -c book [book ...]       check books and exit
  yy                          start empty; press o to open a file

formats: .cbz/.zip, .cbr/.rar, .epub, .fb2/.fb2.zip, .docx (built in);
         .pdf (poppler), .djvu (djvulibre), .doc (antiword);
         the format is detected by signature

flags:
  -r, -renderer kitty|sixel|halfblock
                                  force a renderer (default: detect)
  -c, -check                      index books without opening a reader;
                                  images decode on page access
  -v, -version                    print the version and exit
  -h, -help                       print this help and exit

environment:
  YOYOMI_SHOT_DIR                 where S saves screenshots (default: current dir)
  YOYOMI_EPUB_FALLBACK_FONT       TTF/OTF/TTC with glyphs the bundled fonts lack;
                                  replaces the system font search
  YOYOMI_OCR_LANG                 tesseract language for / search (default: eng)
";

#[derive(Debug, Default, PartialEq)]
struct Args {
    /// The `-r` value as given; empty means detect.
    renderer: String,
    check: bool,
    version: bool,
    help: bool,
    books: Vec<PathBuf>,
}

/// Reads flags: one or two dashes, values after a space or `=`, boolean
/// values only after `=`, and the flags end at the first book, at `--` or at
/// `-h` and `-help`. A boolean is true as `1`, `t`, `T`, `TRUE`, `true` or
/// `True` and false as `0`, `f`, `F`, `FALSE`, `false` or `False`. An error
/// names the flag: bad flag syntax, a flag that needs an argument, a flag
/// not defined, or an invalid boolean value.
fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        if text == "--" {
            parsed.books.extend(args.map(PathBuf::from));
            break;
        }
        let Some(flag) = text.strip_prefix("--").or_else(|| text.strip_prefix('-')).filter(|flag| !flag.is_empty()) else {
            parsed.books.push(PathBuf::from(arg));
            parsed.books.extend(args.map(PathBuf::from));
            break;
        };
        if flag.starts_with(['-', '=']) {
            return Err(format!("bad flag syntax: {text}"));
        }
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        let boolean = || match value {
            None | Some("1" | "t" | "T" | "TRUE" | "true" | "True") => Ok(true),
            Some("0" | "f" | "F" | "FALSE" | "false" | "False") => Ok(false),
            Some(value) => Err(format!("invalid boolean value {value:?} for -{name}: parse error")),
        };
        match name {
            "r" | "renderer" => {
                parsed.renderer = match value {
                    Some(value) => value.to_owned(),
                    None => args.next().ok_or_else(|| format!("flag needs an argument: -{name}"))?.to_string_lossy().into_owned(),
                };
            }
            "c" | "check" => parsed.check = boolean()?,
            "v" | "version" => parsed.version = boolean()?,
            // These names end the flags with the usage, whatever value they carry.
            "h" | "help" => {
                parsed.help = true;
                break;
            }
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }
    Ok(parsed)
}

/// Handles what ends `yy` before any book opens: flag errors and the help on
/// `err`, then the version on `out`, then an unknown renderer. Returns the
/// arguments and the forced renderer, or the exit status.
fn command(argv: impl Iterator<Item = OsString>, out: &mut impl Write, err: &mut impl Write) -> Result<(Args, Option<RendererKind>), u8> {
    let args = match parse(argv) {
        Ok(args) => args,
        Err(message) => {
            let _ = write!(err, "{message}\n{USAGE}");
            return Err(2);
        }
    };
    if args.help {
        let _ = write!(err, "{USAGE}");
        return Err(0);
    }
    if args.version {
        let _ = writeln!(out, "yy {}", env!("CARGO_PKG_VERSION"));
        return Err(0);
    }
    if args.renderer.is_empty() {
        return Ok((args, None));
    }
    match RendererKind::parse(&args.renderer) {
        Some(kind) => Ok((args, Some(kind))),
        None => {
            let _ = writeln!(err, "yy: unknown renderer {:?} (kitty, sixel or halfblock)", args.renderer);
            Err(2)
        }
    }
}

/// Opens each book without a reader and reports its page count. Returns the
/// exit status: 0 when every book opened, 1 when one failed, 2 without books.
fn check_books(books: &[PathBuf], out: &mut impl Write, err: &mut impl Write) -> io::Result<u8> {
    if books.is_empty() {
        writeln!(err, "yy: -check needs at least one book path")?;
        return Ok(2);
    }
    let mut status = 0;
    for path in books {
        let opened = app::open_book(path);
        match opened {
            Ok(Opened::Book(book)) => {
                let pages = book.len();
                book.close();
                writeln!(out, "{path:?}: OK ({pages} pages; indexing only, images not decoded)")?;
            }
            Ok(_) => {
                writeln!(err, "yy: check {path:?}: unsupported book")?;
                status = 1;
            }
            Err(error) => {
                writeln!(err, "yy: check {path:?}: {:?}", error.to_string())?;
                status = 1;
            }
        }
    }
    Ok(status)
}

fn main() -> ExitCode {
    let (args, renderer) = match command(std::env::args_os().skip(1), &mut io::stdout(), &mut io::stderr()) {
        Ok(command) => command,
        Err(status) => return ExitCode::from(status),
    };
    if args.check {
        return match check_books(&args.books, &mut io::stdout(), &mut io::stderr()) {
            Ok(status) => ExitCode::from(status),
            Err(_) => ExitCode::from(1),
        };
    }
    if args.books.len() > 2 {
        eprintln!("yy: at most two books (split screen)");
        return ExitCode::from(2);
    }
    read(&args.books, renderer)
}

/// Reads up to two books in the terminal until a quit key, then restores the
/// terminal, frees its kitty images and saves or forgets the positions.
fn read(books: &[PathBuf], force: Option<RendererKind>) -> ExitCode {
    // Without a terminal every step below fails, so yy stops before writing any of it.
    if let Err(error) = File::options().read(true).write(true).open("/dev/tty") {
        eprintln!("yy: no terminal: open /dev/tty: {error}");
        return ExitCode::from(1);
    }
    // The probe reads the tty itself, so it runs before raw mode.
    let probed = if needs_probe(force) { probe(force == Some(RendererKind::Sixel)) } else { Terminal::default() };
    let renderer = detect(&probed, force);
    let tmux = env::var_os("TMUX").is_some_and(|value| !value.is_empty());
    let resident = Arc::new(Resident::new(ImageIds::for_process(), tmux));
    let (events, received) = unbounded();
    let (view_events, view_received) = unbounded();
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut app = App::new(renderer.clone(), probed, resident.clone(), Positions::load(), events.clone(), view_events, size);
    app.bookmarks = Bookmarks::load();

    // Raw mode is the first change to the terminal; there is nothing to restore before it.
    if let Err(error) = enable_raw_mode() {
        eprintln!("yy: raw mode: {error}");
        return ExitCode::from(1);
    }
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Job threads outlive their panics, so only the loop's own panic
        // gives the terminal back.
        if thread::current().name() == Some("main") {
            restore();
        }
        hook(info);
    }));
    thread::spawn(move || forward_input(crossterm::event::read, Instant::now, &events));
    app.open_books(books);
    // `f` leaves the terminal for the native window, which hands back to it
    // or quits.
    let result = loop {
        let result = run(&mut app, &received, &view_received);
        leave(renderer.as_ref(), &resident);
        if result.is_err() || app.native.is_none() {
            break result;
        }
        if let Some(message) = app.run_native() {
            eprintln!("yy: {message}");
        }
        if app.quit.is_some() {
            break Ok(());
        }
        if let Err(error) = enable_raw_mode() {
            break Err(io::Error::new(error.kind(), format!("raw mode: {error}")));
        }
    };
    if let Err(error) = result {
        eprintln!("yy: {error}");
        return ExitCode::from(1);
    }
    if let Some(error) = app.finish() {
        eprintln!("yy: {error}");
    }
    ExitCode::SUCCESS
}

/// Takes over the terminal, already in raw mode, and runs the event loop
/// until a quit key or `f`: input from a thread that reads the terminal,
/// finished view jobs, and the timers of eased scrolling and resizes. Every
/// turn of the loop takes the terminal's size and writes a frame.
fn run(app: &mut App, received: &Receiver<Event>, view_received: &Receiver<ViewEvent>) -> io::Result<()> {
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(BufWriter::new(io::stdout())))?;
    loop {
        if let Some(error) = app.input_error.take() {
            return Err(io::Error::new(error.kind(), format!("read terminal input: {error}")));
        }
        follow_size(app, crossterm::terminal::size(), cell_size(app.probed()));
        draw::write_frame(&mut terminal, app)?;
        if app.quit.is_some() || app.native.is_some() {
            return Ok(());
        }
        let timer = app.deadline().map_or_else(never, at);
        select! {
            recv(received) -> event => if let Ok(event) = event {
                app.on_event(event);
            },
            recv(view_received) -> event => if let Ok(event) = event {
                app.on_view_event(event);
            },
            recv(timer) -> _ => app.on_timer(Instant::now()),
        }
    }
}

/// Hands `app` the terminal size `size` read as a resize when it differs
/// from the last size the app was given, or when the cell size `cell` read
/// with it differs from the renderer's. A resize signal sent before
/// crossterm listens for it is lost, as when a terminal grows its window
/// or its pixels (WezTerm, right after it starts) while the reader starts;
/// this check repairs the layout on the next turn.
fn follow_size(app: &mut App, size: io::Result<(u16, u16)>, cell: CellSize) {
    if let Ok((cols, rows)) = size
        && ((cols, rows) != (app.cols, app.rows) || cell != app.cell())
    {
        app.on_event(Event::Input(Input::Resize(cols, rows)));
    }
}

/// How long a terminal string may take to end, from its introducer's read
/// time. A reply over a slow link can come in pieces; the limit keeps a
/// stray introducer typed on the keyboard from dropping keys for longer.
const REPLY_TIME: Duration = Duration::from_secs(1);

/// Sends terminal input to the loop until the loop is gone, without the keys
/// of terminal strings, which `terminal_reply` tells by the time `now` gives
/// right after each read. A read error goes to the loop as well and ends the
/// input.
fn forward_input(mut read: impl FnMut() -> io::Result<Input>, mut now: impl FnMut() -> Instant, events: &Sender<Event>) {
    let mut reply = None;
    loop {
        let event = match read() {
            Ok(Input::Key(key)) if terminal_reply(&mut reply, key, now()) => continue,
            Ok(input) => Event::Input(input),
            Err(error) => {
                let _ = events.send(Event::InputFailed(error));
                return;
            }
        };
        if events.send(event).is_err() {
            return;
        }
    }
}

/// Whether `key`, read at `at`, belongs to a terminal string, such as a reply
/// to a graphics command or a late reply to the startup probe. crossterm
/// reads the APC, DCS, OSC and PM introducers `ESC _`, `ESC P`, `ESC ]` and
/// `ESC ^` as alt keys, the string as more keys, and its end as alt+`\`
/// or, for an OSC, ctrl+`g`. `reply` holds when the string being dropped
/// began and whether it is an OSC. The reader binds no alt keys, so an
/// introducer always drops; a string that has not ended within `REPLY_TIME`
/// gives the keys after it back.
fn terminal_reply(reply: &mut Option<(Instant, bool)>, key: KeyEvent, at: Instant) -> bool {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if let Some((began, osc)) = *reply
        && at < began + REPLY_TIME
    {
        let bel = osc && key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL);
        if bel || (alt && key.code == KeyCode::Char('\\')) {
            *reply = None;
        }
        return true;
    }
    *reply = None;
    let KeyCode::Char(introducer @ ('_' | 'P' | ']' | '^')) = key.code else { return false };
    if alt {
        *reply = Some((at, introducer == ']'));
    }
    alt
}

/// Leaves the terminal as quitting does: puts it back and frees the kitty
/// images this process may hold.
fn leave(renderer: &dyn Renderer, resident: &Resident) {
    restore();
    let mut out = io::stdout().lock();
    for id in resident.ids() {
        let _ = out.write_all(&renderer.delete(id));
    }
    let _ = out.flush();
}

/// Puts the terminal back as the shell had it. A frame cut short by an error
/// or a panic may still hold a synchronized update open, so it ends first.
fn restore() {
    let _ = execute!(io::stdout(), EndSynchronizedUpdate, DisableMouseCapture, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(OsString::from))
    }

    #[test]
    fn flags_take_one_or_two_dashes_and_values_after_a_space_or_equals() {
        let parsed = args(&["-r", "kitty", "--check", "a.cbz", "b.cbr"]).unwrap();
        assert_eq!(parsed.renderer, "kitty");
        assert!(parsed.check);
        assert_eq!(parsed.books, [PathBuf::from("a.cbz"), PathBuf::from("b.cbr")]);
        assert_eq!(args(&["-renderer=sixel"]).unwrap().renderer, "sixel");
        assert!(args(&["-v"]).unwrap().version && args(&["-help"]).unwrap().help);
    }

    #[test]
    fn flags_end_at_the_first_book() {
        let parsed = args(&["a.cbz", "-c"]).unwrap();
        assert!(!parsed.check);
        assert_eq!(parsed.books, [PathBuf::from("a.cbz"), PathBuf::from("-c")]);
        assert_eq!(args(&["--", "-r"]).unwrap().books, [PathBuf::from("-r")]);
    }

    /// The result of `command` on `list` without the arguments, then what
    /// it wrote to stdout and to stderr.
    fn run_command(list: &[&str]) -> (Result<Option<RendererKind>, u8>, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = command(list.iter().map(OsString::from), &mut out, &mut err).map(|(_, renderer)| renderer);
        (result, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn bad_flags_fail() {
        assert_eq!(args(&["-r"]).unwrap_err(), "flag needs an argument: -r");
        assert_eq!(args(&["-renderer"]).unwrap_err(), "flag needs an argument: -renderer");
        assert_eq!(args(&["-x"]).unwrap_err(), "flag provided but not defined: -x");
        assert_eq!(args(&["---x"]).unwrap_err(), "bad flag syntax: ---x");
        assert_eq!(args(&["-=x"]).unwrap_err(), "bad flag syntax: -=x");
        let (result, out, err) = run_command(&["-x"]);
        assert_eq!((result, out.as_str()), (Err(2), ""));
        assert_eq!(err, format!("flag provided but not defined: -x\n{USAGE}"));
    }

    #[test]
    fn boolean_flags_take_six_spellings_of_true_and_of_false() {
        let values = [("1", true), ("t", true), ("T", true), ("TRUE", true), ("true", true), ("True", true)].into_iter().chain([
            ("0", false),
            ("f", false),
            ("F", false),
            ("FALSE", false),
            ("false", false),
            ("False", false),
        ]);
        for (value, want) in values {
            for name in ["-c", "-check", "--check"] {
                assert_eq!(args(&[&format!("{name}={value}")]).unwrap().check, want, "{name}={value}");
            }
            for name in ["-v", "-version"] {
                assert_eq!(args(&[&format!("{name}={value}")]).unwrap().version, want, "{name}={value}");
            }
        }
        assert!(!args(&["-check", "-check=false"]).unwrap().check);
        assert_eq!(args(&["-check", "false"]).unwrap().books, [PathBuf::from("false")]);
        for bad in ["-check=garbage", "-c=", "-version=yes", "-v=2"] {
            let (result, out, err) = run_command(&[bad]);
            assert_eq!((result, out.as_str()), (Err(2), ""), "{bad}");
            assert!(err.starts_with("invalid boolean value ") && err.ends_with(USAGE), "{bad}: {err}");
        }
        assert!(run_command(&["-check=garbage"]).2.starts_with("invalid boolean value \"garbage\" for -check: parse error\n"));
    }

    #[test]
    fn version_comes_before_the_renderer_check() {
        let (result, out, err) = run_command(&["-r", "bogus", "-version"]);
        assert_eq!((result, out.as_str(), err.as_str()), (Err(0), concat!("yy ", env!("CARGO_PKG_VERSION"), "\n"), ""));
        let (result, out, err) = run_command(&["-r", "bogus", "-version=false"]);
        assert_eq!((result, out.as_str(), err.as_str()), (Err(2), "", "yy: unknown renderer \"bogus\" (kitty, sixel or halfblock)\n"));
        assert_eq!(run_command(&["-r", "kitty"]).0, Ok(Some(RendererKind::Kitty)));
        assert_eq!(run_command(&["-r="]).0, Ok(None));
    }

    #[test]
    fn help_stops_parsing_where_it_stands() {
        let lists: [&[&str]; 7] =
            [&["-help", "-bogus"], &["-h"], &["--help"], &["-help=false"], &["-h=0"], &["-h=garbage"], &["-r", "bogus", "-h", "-v"]];
        for list in lists {
            let (result, out, err) = run_command(list);
            assert_eq!((result, out.as_str(), err.as_str()), (Err(0), "", USAGE), "{list:?}");
        }
        assert_eq!(run_command(&["-bogus", "-help"]).0, Err(2));
        assert_eq!(args(&["-c", "-h", "a.cbz"]).unwrap(), Args { check: true, help: true, ..Args::default() });
    }

    #[test]
    fn input_errors_reach_the_loop() {
        let (events, received) = unbounded();
        let mut inputs = vec![Ok(Input::FocusGained), Err(io::Error::other("tty gone"))].into_iter();
        forward_input(|| inputs.next().expect("input stops after the error"), Instant::now, &events);
        assert!(matches!(received.try_recv(), Ok(Event::Input(Input::FocusGained))));
        let mut app = crate::app::tests::reader(2);
        app.on_event(received.try_recv().unwrap());
        assert_eq!(app.input_error.map(|error| error.to_string()).as_deref(), Some("tty gone"));
    }

    #[test]
    fn each_turn_hands_a_changed_terminal_size_to_the_app_once() {
        let mut app = crate::app::tests::reader(5);
        app.sixel_fresh = false;
        follow_size(&mut app, Ok((80, 24)), CellSize::FALLBACK);
        assert!(app.deadline().is_none() && !app.sixel_fresh, "the same size starts no timer and no frame");
        follow_size(&mut app, Err(io::Error::other("no tty")), CellSize::FALLBACK);
        assert!(app.deadline().is_none() && !app.sixel_fresh, "a failed read changes nothing");

        follow_size(&mut app, Ok((110, 36)), CellSize::FALLBACK);
        assert_eq!((app.cols, app.rows), (110, 36));
        let settles = app.deadline().expect("the resize settles later");
        app.sixel_fresh = false;
        thread::sleep(Duration::from_millis(5));
        follow_size(&mut app, Ok((110, 36)), CellSize::FALLBACK);
        assert_eq!(app.deadline(), Some(settles), "turns at the new size keep the settle time");
        assert!(!app.sixel_fresh);
    }

    #[test]
    fn each_turn_hands_a_changed_cell_size_to_the_app_once() {
        // A cell no tty gives here, so the cell read differs from it.
        let renderer = Arc::new(yoyomi_render::HalfBlock::new(CellSize { w: 7.5, h: 15.5 }));
        let (mut app, _) = crate::app::tests::app_with(renderer, 80, 24);
        crate::app::tests::deliver(&mut app, 0, "/books/fake.cbz", Ok(crate::app::tests::fake(5, "/books/fake.cbz")));
        app.sixel_fresh = false;
        let cell = cell_size(app.probed());
        assert_ne!(cell, app.cell());
        follow_size(&mut app, Err(io::Error::other("no tty")), cell);
        assert!(app.deadline().is_none() && !app.sixel_fresh, "a failed read changes nothing");

        follow_size(&mut app, Ok((80, 24)), cell);
        assert_eq!((app.cols, app.rows, app.cell()), (80, 24, cell), "the renderer takes the cell read");
        let settles = app.deadline().expect("the resize settles later");
        assert!(app.sixel_fresh, "the resize lays out again");
        app.sixel_fresh = false;
        thread::sleep(Duration::from_millis(5));
        follow_size(&mut app, Ok((80, 24)), cell);
        assert_eq!(app.deadline(), Some(settles), "turns with the new cell keep the settle time");
        assert!(!app.sixel_fresh, "turns with the new cell lay out nothing");
    }

    /// The keys `forward_input` queues from `keys`, each read at its time
    /// after the start.
    fn forwarded(keys: &[(KeyEvent, u64)]) -> Vec<KeyEvent> {
        let start = Instant::now();
        let (events, received) = unbounded();
        let mut reads = keys.iter().map(|&(key, _)| Ok(Input::Key(key))).chain([Err(io::Error::other("end"))]);
        let mut times = keys.iter().map(|&(_, ms)| start + Duration::from_millis(ms));
        forward_input(|| reads.next().expect("input stops after the error"), || times.next().expect("a time per key"), &events);
        received.try_iter().filter_map(|event| if let Event::Input(Input::Key(key)) = event { Some(key) } else { None }).collect()
    }

    fn plain(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    #[test]
    fn terminal_replies_never_reach_the_loop() {
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        // A kitty reply in pieces that end just inside the second, then an OSC
        // ended by BEL (ctrl+g) and a DCS (alt+shift+P), each read in one burst.
        let mut burst = vec![(alt('_'), 0)];
        burst.extend("Gi=1".chars().map(|c| (plain(c), 0)));
        burst.extend(";OK".chars().map(|c| (plain(c), 600)));
        burst.extend([(alt('\\'), 999), (alt(']'), 1000)]);
        burst.extend("11;rgb:0/0/0".chars().map(|c| (plain(c), 1000)));
        burst.extend([(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL), 1000)]);
        burst.extend([(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::ALT | KeyModifiers::SHIFT), 1001), (plain('r'), 1001), (alt('\\'), 1001)]);
        burst.push((plain('j'), 1002));
        let queued = forwarded(&burst);
        assert_eq!(queued, [plain('j')], "only the key after the strings");
        // The loop takes the queue whenever it can; only read times decided what it holds.
        let mut app = crate::app::tests::reader(5);
        thread::sleep(REPLY_TIME);
        for key in queued {
            app.on_event(Event::Input(Input::Key(key)));
        }
        assert!(app.title(0).starts_with("Fake  2/5"), "{}", app.title(0));
    }

    #[test]
    fn an_unended_terminal_string_gives_keys_back_after_its_time() {
        let pm = KeyEvent::new(KeyCode::Char('^'), KeyModifiers::ALT);
        let keys = [(pm, 0), (plain('G'), 500), (plain('i'), 999), (plain('j'), 1000), (plain('k'), 1001)];
        assert_eq!(forwarded(&keys), [plain('j'), plain('k')]);
    }

    #[test]
    fn check_reports_each_book() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(check_books(&[], &mut out, &mut err).unwrap(), 2);
        let status = check_books(&[PathBuf::from("/nonexistent/book.cbz")], &mut out, &mut err).unwrap();
        assert_eq!(status, 1);
        assert!(String::from_utf8(err).unwrap().contains("book.cbz"));
    }
}
