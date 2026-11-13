//! The startup probe and the choice of renderer.

mod probe;

use std::fs::{File, OpenOptions};
use std::process::{Command, Stdio};
use std::sync::Arc;

use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcgetwinsize, tcsetattr};
use yoyomi_core::{CellSize, Renderer, RendererKind};

use crate::{HalfBlock, Kitty, Sixel};

/// What the startup probe learned about the terminal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Terminal {
    /// The cell size the terminal reported through XTWINOPS.
    pub cell: Option<CellSize>,
    /// The terminal's device attributes list sixel graphics.
    pub sixel: bool,
    /// The sixel color registers the terminal reported (XTSMGRAPHICS), or 0.
    pub colors: u16,
    /// The terminal accepted a kitty graphics query, as herdr panes do.
    pub kitty: bool,
    /// The background color iTerm2 reported (OSC 11) with its color registers.
    pub background: Option<[u8; 3]>,
}

/// Reads one environment variable, empty when unset.
type Env<'a> = &'a dyn Fn(&str) -> String;

fn process_env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn open_tty() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

/// Asks the terminal for its cell size and graphics support. Run it before
/// raw mode; it reads the replies from the tty itself. With `sixel`, it also
/// asks for color registers when the attributes do not list sixel.
pub fn probe(sixel: bool) -> Terminal {
    let Ok(mut tty) = open_tty() else { return Terminal::default() };
    let Ok(old) = tcgetattr(&tty) else { return Terminal::default() };
    let mut raw = old.clone();
    raw.local_modes.remove(LocalModes::ICANON | LocalModes::ECHO);
    raw.special_codes[SpecialCodeIndex::VMIN] = 0;
    // A read returns after 300 ms of silence.
    raw.special_codes[SpecialCodeIndex::VTIME] = 3;
    if tcsetattr(&tty, OptionalActions::Now, &raw).is_err() {
        return Terminal::default();
    }
    let terminal = probe::query(&mut tty, sixel, in_herdr(&process_env), in_iterm2(&process_env));
    let _ = tcsetattr(&tty, OptionalActions::Now, &old);
    terminal
}

/// Whether the probe must run before [`detect`]: false only when half blocks
/// are forced and TIOCGWINSZ reports pixel sizes, since half blocks need the
/// cell size only for its proportions.
pub fn needs_probe(force: Option<RendererKind>) -> bool {
    force != Some(RendererKind::HalfBlock) || winsize_cell().is_none()
}

/// Picks a renderer: `force` when given, otherwise kitty where the terminal
/// speaks it, sixel where its device attributes list it, and half blocks
/// elsewhere. Under tmux it turns on passthrough for the kitty renderer.
pub fn detect(terminal: &Terminal, force: Option<RendererKind>) -> Arc<dyn Renderer> {
    let cell = cell_size(terminal);
    match choose(terminal, force, &process_env, tmux_client) {
        RendererKind::Kitty => {
            let tmux = !process_env("TMUX").is_empty();
            if tmux {
                allow_passthrough(|value| {
                    Command::new("tmux")
                        .args(["set", "-p", "allow-passthrough", value])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                });
            }
            Arc::new(Kitty::new(cell, tmux))
        }
        RendererKind::Sixel => Arc::new(Sixel::new(cell, terminal.colors).with_background(terminal.background)),
        _ => Arc::new(HalfBlock::new(cell)),
    }
}

/// Sets the pane's `allow-passthrough` through `set`, which reports success.
/// tmux drops APC sequences unless passthrough is on (tmux 3.3 and later).
/// With `on` it also drops one that arrives while it redraws the window, so
/// `all` comes first; tmux 3.3 knows only `on` and `off`.
fn allow_passthrough(mut set: impl FnMut(&str) -> bool) {
    if !set("all") {
        set("on");
    }
}

/// The current cell size: the probe's answer, or the tty's own pixel size
/// (TIOCGWINSZ) when that is smaller, since it follows font changes. In
/// iTerm2 and WezTerm the tty's size alone counts.
pub fn cell_size(terminal: &Terminal) -> CellSize {
    choose_cell(&process_env, terminal.cell, winsize_cell())
}

/// iTerm2 and WezTerm draw one sixel pixel as one device pixel, and their tty
/// reports device pixels, while their XTWINOPS reply speaks points (iTerm2) or
/// the size of the moment it was asked (WezTerm, which corrects its tty size
/// twice right after it starts). There the tty's cell wins when it has one;
/// under a multiplexer the tty's pixels are the multiplexer's.
fn choose_cell(env: Env, probed: Option<CellSize>, winsize: Option<CellSize>) -> CellSize {
    match winsize {
        Some(tty) if outside_multiplexer(env) && matches!(env("TERM_PROGRAM").as_str(), "iTerm.app" | "WezTerm") => tty,
        _ => pick_cell(probed, winsize),
    }
}

/// The probe's answer comes from the terminal itself, while TIOCGWINSZ passes
/// through multiplexers that may report a whole window for one pane. A
/// smaller TIOCGWINSZ size wins because it follows font changes, sixel pixels
/// map one to one onto the screen, and cells taken too large push an image
/// past its rows.
// shortcut: a font that grows keeps the startup size, and so does one that
// shrinks where TIOCGWINSZ has no pixel sizes; asking XTWINOPS again on resize
// would cover both.
fn pick_cell(probed: Option<CellSize>, winsize: Option<CellSize>) -> CellSize {
    match (probed, winsize) {
        (Some(p), Some(t)) => CellSize { w: p.w.min(t.w), h: p.h.min(t.h) },
        (Some(c), None) => c,
        // TIOCGWINSZ may count the terminal's inner border (xterm adds 2 px a
        // side); a cell a fraction too small shrinks an image, one too large spills.
        (None, Some(t)) => CellSize { w: t.w.floor().max(1.0), h: t.h.floor().max(1.0) },
        (None, None) => CellSize::FALLBACK,
    }
}

/// The cell size from the tty's TIOCGWINSZ pixel fields, when it has them.
fn winsize_cell() -> Option<CellSize> {
    let ws = tcgetwinsize(open_tty().ok()?).ok()?;
    let sizes = [ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel];
    (!sizes.contains(&0)).then(|| CellSize { w: f64::from(ws.ws_xpixel) / f64::from(ws.ws_col), h: f64::from(ws.ws_ypixel) / f64::from(ws.ws_row) })
}

/// The terminal tmux's attached client runs in, as tmux names it.
#[derive(Debug)]
struct TmuxClient {
    /// The client's TERM.
    termname: String,
    /// The terminal's name from its XTVERSION reply, such as `kitty(0.41.1)`.
    /// Empty when the terminal did not answer, before tmux 3.2, and for a
    /// moment after the client attaches.
    termtype: String,
}

fn tmux_client() -> Option<TmuxClient> {
    let out = Command::new("tmux").args(["display-message", "-p", "#{client_termname}\t#{client_termtype}"]).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (termname, termtype) = text.split_once('\t').unwrap_or((&text, ""));
    Some(TmuxClient { termname: termname.trim().to_owned(), termtype: termtype.trim().to_owned() })
}

fn choose(terminal: &Terminal, force: Option<RendererKind>, env: Env, tmux_client: impl FnOnce() -> Option<TmuxClient>) -> RendererKind {
    match force {
        Some(kind) => kind,
        None if supports_kitty(env, terminal.kitty, tmux_client) => RendererKind::Kitty,
        None if terminal.sixel => RendererKind::Sixel,
        None => RendererKind::HalfBlock,
    }
}

/// Whether yoyomi runs in a herdr pane with no multiplexer in between. herdr
/// names itself in place of the terminal around it and draws kitty graphics
/// for its panes, so the probe asks it with a kitty graphics query. A
/// multiplexer inside the pane keeps herdr's name but does not pass the query
/// on, and GNU screen or an old tmux would show it as a status line or title.
fn in_herdr(env: Env) -> bool {
    outside_multiplexer(env) && env("TERM_PROGRAM") == "herdr"
}

/// Whether yoyomi runs in iTerm2 with no multiplexer in between. iTerm2 fills
/// the pixels a sixel image leaves unpainted, so the probe asks it for its
/// background color.
fn in_iterm2(env: Env) -> bool {
    outside_multiplexer(env) && env("TERM_PROGRAM") == "iTerm.app"
}

/// Whether none of GNU screen, tmux and zellij runs between yoyomi and the
/// terminal its TERM_PROGRAM names.
fn outside_multiplexer(env: Env) -> bool {
    ["STY", "TMUX", "ZELLIJ"].iter().all(|v| env(v).is_empty())
}

/// Whether the terminal likely speaks the kitty graphics protocol with
/// Unicode placeholders, as kitty, Ghostty and herdr do. Under tmux, the
/// terminal hosting the tmux client counts.
fn supports_kitty(env: Env, kitty_answered: bool, tmux_client: impl FnOnce() -> Option<TmuxClient>) -> bool {
    // shortcut: zellij keeps kitty's TERM but drops its graphics, and a terminal
    // started from inside zellij inherits ZELLIJ too; `-r kitty` covers that case.
    if !env("ZELLIJ").is_empty() {
        return false;
    }
    if in_herdr(env) {
        // shortcut: herdr accepts the query even when the terminal around it has
        // no kitty graphics and cannot be asked about it; pages stay blank there
        // until `-r halfblock`.
        return kitty_answered;
    }
    // GNU screen keeps the outer TERM_PROGRAM and TMUX; only its own TERM counts.
    let screen = !env("STY").is_empty();
    let program = if screen { "screen".to_owned() } else { env("TERM_PROGRAM") };
    if term_suggests_kitty(&env("TERM"), &program) {
        return true;
    }
    if !screen
        && !env("TMUX").is_empty()
        && let Some(client) = tmux_client()
    {
        if term_suggests_kitty(&client.termname.to_lowercase(), "") || term_suggests_kitty(&client.termtype.to_lowercase(), "") {
            return true;
        }
        // A tmux session keeps the markers of the terminal it started in, while
        // its client may since have attached from another one, which the type names.
        if !client.termtype.is_empty() {
            return false;
        }
    }
    // Markers inherited from the outer terminal. Terminals started from kitty,
    // such as VS Code's, inherit them as well but name themselves in
    // TERM_PROGRAM.
    let markers = ["KITTY_WINDOW_ID", "KITTY_PID", "GHOSTTY_RESOURCES_DIR", "GHOSTTY_BIN_DIR"];
    (program.is_empty() || program == "tmux") && markers.iter().any(|v| !env(v).is_empty())
}

fn term_suggests_kitty(term: &str, program: &str) -> bool {
    term.contains("kitty") || term.contains("ghostty") || program == "ghostty"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables as name and value pairs.
    type Vars = &'static [(&'static str, &'static str)];

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> String {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    fn no_tmux_client() -> Option<TmuxClient> {
        panic!("tmux asked outside tmux")
    }

    #[test]
    fn supports_kitty_ignores_inherited_markers() {
        let cases: [(&str, Vars, bool, bool); 10] = [
            ("kitty", &[("TERM", "xterm-kitty")], false, true),
            ("Ghostty with a plain TERM", &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "ghostty")], false, true),
            ("kitty with a plain TERM", &[("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")], false, true),
            ("VS Code started from kitty", &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "vscode"), ("KITTY_WINDOW_ID", "1")], false, false),
            ("zellij in kitty", &[("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1"), ("ZELLIJ", "0")], false, false),
            (
                "screen in Ghostty",
                &[("TERM", "screen"), ("TERM_PROGRAM", "ghostty"), ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty"), ("STY", "1.pts-0")],
                false,
                false,
            ),
            ("kitty started from screen", &[("TERM", "xterm-kitty"), ("STY", "1.pts-0")], false, true),
            ("herdr", &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "herdr")], true, true),
            ("herdr with kitty graphics off", &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "herdr")], false, false),
            (
                "herdr started from Ghostty",
                &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "herdr"), ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")],
                false,
                false,
            ),
        ];
        for (name, vars, answered, want) in cases {
            assert_eq!(supports_kitty(&env(vars), answered, no_tmux_client), want, "{name}");
        }
    }

    #[test]
    fn supports_kitty_asks_tmux_for_its_client_terminal() {
        const TMUX: Vars = &[("TERM", "tmux-256color"), ("TERM_PROGRAM", "tmux"), ("TMUX", "/tmp/tmux-1000/default,1,0")];
        const GHOSTTY: Vars = &[
            ("TERM", "tmux-256color"),
            ("TERM_PROGRAM", "tmux"),
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty"),
        ];
        const KITTY: Vars = &[("TERM", "tmux-256color"), ("TERM_PROGRAM", "tmux"), ("TMUX", "/tmp/tmux-1000/default,1,0"), ("KITTY_WINDOW_ID", "1")];
        let cases = [
            ("Ghostty with a plain TERM", GHOSTTY, Some(("xterm-256color", "ghostty 1.3.2-HEAD-+cb1ef70")), true),
            ("Ghostty with no type yet", GHOSTTY, Some(("xterm-256color", "")), true),
            ("started in Ghostty, attached from iTerm2", GHOSTTY, Some(("xterm-256color", "iTerm2 3.6.8")), false),
            ("started in Ghostty, attached from xterm", GHOSTTY, Some(("xterm-256color", "XTerm(398)")), false),
            ("started in Ghostty, attached from tmux", GHOSTTY, Some(("tmux-256color", "tmux 3.5a")), false),
            ("started in kitty, attached from xterm", KITTY, Some(("xterm-256color", "XTerm(398)")), false),
            ("started in kitty, no type yet", KITTY, Some(("xterm-256color", "")), true),
            ("plain terminal", TMUX, Some(("xterm-256color", "")), false),
            ("kitty type", TMUX, Some(("xterm-256color", "kitty(0.41.1)")), true),
            ("Ghostty type in capitals", TMUX, Some(("xterm-256color", "Ghostty 1.2.0")), true),
            ("kitty TERM", TMUX, Some(("xterm-kitty", "")), true),
            ("Ghostty TERM", TMUX, Some(("xterm-ghostty", "")), true),
            ("no answer, Ghostty markers", GHOSTTY, None, true),
            ("no answer, no markers", TMUX, None, false),
        ];
        for (name, vars, answer, want) in cases {
            let client = || answer.map(|(termname, termtype)| TmuxClient { termname: termname.into(), termtype: termtype.into() });
            assert_eq!(supports_kitty(&env(vars), false, client), want, "{name}");
        }
    }

    #[test]
    fn supports_kitty_skips_tmux_under_screen() {
        let screen_in_tmux = [("TERM", "screen"), ("TERM_PROGRAM", "tmux"), ("TMUX", "/tmp/tmux-1000/default,1,0"), ("STY", "1.pts-0")];
        assert!(!supports_kitty(&env(&screen_in_tmux), false, no_tmux_client));
        let kitty_in_screen = [("TERM", "xterm-kitty"), ("TMUX", "/tmp/tmux-1000/default,1,0"), ("STY", "1.pts-0")];
        assert!(supports_kitty(&env(&kitty_in_screen), false, no_tmux_client), "kitty started from screen in tmux");
    }

    // A multiplexer inside a herdr pane must not get the kitty graphics query.
    // GNU screen and tmux show an unknown APC string as a status line or title.
    #[test]
    fn kitty_query_goes_to_herdr_panes_only() {
        let cases: [(&str, Vars, bool); 6] = [
            ("herdr", &[("TERM_PROGRAM", "herdr")], true),
            ("Ghostty", &[("TERM_PROGRAM", "ghostty")], false),
            ("no terminal name", &[], false),
            ("screen in herdr", &[("TERM_PROGRAM", "herdr"), ("STY", "1.pts-0")], false),
            ("tmux in herdr", &[("TERM_PROGRAM", "herdr"), ("TMUX", "/tmp/tmux-1000/default,1,0")], false),
            ("zellij in herdr", &[("TERM_PROGRAM", "herdr"), ("ZELLIJ", "0")], false),
        ];
        for (name, vars, want) in cases {
            assert_eq!(in_herdr(&env(vars)), want, "{name}");
        }
    }

    #[test]
    fn background_query_goes_to_iterm2_only() {
        let cases: [(&str, Vars, bool); 5] = [
            ("iTerm2", &[("TERM_PROGRAM", "iTerm.app")], true),
            ("WezTerm", &[("TERM_PROGRAM", "WezTerm")], false),
            ("no terminal name", &[], false),
            ("tmux in iTerm2", &[("TERM_PROGRAM", "iTerm.app"), ("TMUX", "/tmp/tmux-501/default,1,0")], false),
            ("screen in iTerm2", &[("TERM_PROGRAM", "iTerm.app"), ("STY", "1.ttys000")], false),
        ];
        for (name, vars, want) in cases {
            assert_eq!(in_iterm2(&env(vars)), want, "{name}");
        }
    }

    #[test]
    fn choose_prefers_force_then_kitty_then_sixel() {
        let kitty = env(&[("TERM", "xterm-kitty")]);
        let plain = env(&[("TERM", "xterm-256color")]);
        let sixel = Terminal { sixel: true, ..Terminal::default() };
        for force in [RendererKind::Kitty, RendererKind::Sixel, RendererKind::HalfBlock] {
            assert_eq!(choose(&sixel, Some(force), &kitty, no_tmux_client), force);
            assert_eq!(choose(&Terminal::default(), Some(force), &plain, no_tmux_client), force);
        }
        assert_eq!(choose(&sixel, None, &kitty, no_tmux_client), RendererKind::Kitty);
        assert_eq!(choose(&sixel, None, &plain, no_tmux_client), RendererKind::Sixel);
        assert_eq!(choose(&Terminal::default(), None, &plain, no_tmux_client), RendererKind::HalfBlock);
    }

    #[test]
    fn allow_passthrough_tries_all_then_on() {
        for (all_works, want) in [(true, ["all"].as_slice()), (false, &["all", "on"])] {
            let mut tried = Vec::new();
            allow_passthrough(|value| {
                tried.push(value.to_owned());
                value != "all" || all_works
            });
            assert_eq!(tried, want, "all accepted: {all_works}");
        }
    }

    #[test]
    fn detect_builds_the_forced_renderer() {
        for force in [RendererKind::Sixel, RendererKind::HalfBlock] {
            assert_eq!(detect(&Terminal::default(), Some(force)).kind(), force);
        }
    }

    #[test]
    fn needs_probe_unless_half_blocks_have_a_window_size() {
        assert!(needs_probe(None));
        assert!(needs_probe(Some(RendererKind::Kitty)));
        assert!(needs_probe(Some(RendererKind::Sixel)));
        assert_eq!(needs_probe(Some(RendererKind::HalfBlock)), winsize_cell().is_none());
    }

    #[test]
    fn cell_size_follows_a_shrunk_font() {
        let cell = |w, h| Some(CellSize { w, h });
        for (name, probed, winsize, want) in [
            ("font shrunk after startup", cell(8.0, 16.0), cell(7.0, 14.0), CellSize { w: 7.0, h: 14.0 }),
            ("mux reports the whole window", cell(8.0, 16.0), cell(16.0, 32.0), CellSize { w: 8.0, h: 16.0 }),
            ("no window size", cell(8.0, 16.0), None, CellSize { w: 8.0, h: 16.0 }),
            ("no probe answer", None, cell(9.0, 18.0), CellSize { w: 9.0, h: 18.0 }),
            ("xterm counts its border", None, cell(1534.0 / 170.0, 954.0 / 50.0), CellSize { w: 9.0, h: 19.0 }),
            ("under one pixel", None, cell(0.5, 0.5), CellSize { w: 1.0, h: 1.0 }),
            ("nothing known", None, None, CellSize::FALLBACK),
        ] {
            assert_eq!(pick_cell(probed, winsize), want, "{name}");
        }
    }

    #[test]
    fn cell_size_is_the_ttys_in_iterm2_and_wezterm() {
        let cell = |w, h| CellSize { w, h };
        const ITERM: Vars = &[("TERM_PROGRAM", "iTerm.app")];
        const WEZTERM: Vars = &[("TERM_PROGRAM", "WezTerm")];
        // The XTWINOPS reply in iTerm2, and in WezTerm's first moments.
        const POINTS: Option<CellSize> = Some(CellSize { w: 8.0, h: 16.0 });
        // iTerm2's tty at scale 2.
        const RETINA: Option<CellSize> = Some(CellSize { w: 16.0, h: 32.0 });
        // A name, the environment, the probed cell, the tty's cell and the cell wanted.
        type Case = (&'static str, Vars, Option<CellSize>, Option<CellSize>, CellSize);
        let cases: [Case; 13] = [
            ("iTerm2 at scale 2", ITERM, POINTS, RETINA, cell(16.0, 32.0)),
            ("iTerm2 at scale 1", ITERM, POINTS, POINTS, cell(8.0, 16.0)),
            ("WezTerm settled at scale 2 after the probe", WEZTERM, POINTS, Some(cell(16.0, 35.0)), cell(16.0, 35.0)),
            ("WezTerm settled at scale 1 after the probe", WEZTERM, POINTS, Some(cell(8.0, 18.0)), cell(8.0, 18.0)),
            ("iTerm2 after a font change to 12x24", ITERM, POINTS, Some(cell(12.0, 24.0)), cell(12.0, 24.0)),
            ("iTerm2 after a font change to 20x40", ITERM, POINTS, Some(cell(20.0, 40.0)), cell(20.0, 40.0)),
            ("another terminal", &[("TERM_PROGRAM", "Apple_Terminal")], POINTS, RETINA, cell(8.0, 16.0)),
            ("tmux in iTerm2", &[("TERM_PROGRAM", "iTerm.app"), ("TMUX", "/tmp/tmux-501/default,1,0")], POINTS, RETINA, cell(8.0, 16.0)),
            ("screen in iTerm2", &[("TERM_PROGRAM", "iTerm.app"), ("STY", "1.ttys000")], POINTS, RETINA, cell(8.0, 16.0)),
            ("zellij in iTerm2", &[("TERM_PROGRAM", "iTerm.app"), ("ZELLIJ", "0")], POINTS, RETINA, cell(8.0, 16.0)),
            ("iTerm2 with no tty pixels", ITERM, POINTS, None, cell(8.0, 16.0)),
            ("WezTerm with no tty pixels", WEZTERM, POINTS, None, cell(8.0, 16.0)),
            ("iTerm2 with nothing known", ITERM, None, None, CellSize::FALLBACK),
        ];
        for (name, vars, probed, winsize, want) in cases {
            assert_eq!(choose_cell(&env(vars), probed, winsize), want, "{name}");
        }
    }
}
