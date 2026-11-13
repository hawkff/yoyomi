//! Frames: the reader's state drawn into a ratatui buffer, and the bytes each
//! frame sends to the terminal.

use std::io::{self, Write};
use std::num::NonZeroU16;
use std::sync::Arc;

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{Buffer, Cell, CellDiffOption};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::{Frame, Terminal};
use yoyomi_core::{At, Grid, ImageId, Layer, Rgb, Scene, Upload};
use yoyomi_render::{MAX_CELLS, placeholder};

use crate::app::{App, ImageBox};
use crate::search;

/// The shortcut tips on the right of the status bar.
const TIPS: &str = "j/k page e in-browser s spread R rotate tab chapters  ? help  q quit ";

const HELP: &str = "\
yy: terminal reader (.cbz/.cbr/.epub/.fb2/.docx/.doc/.pdf/.djvu)
Book text: images with kitty or sixel graphics, plain text elsewhere (webtoon needs images).

  j / k          next / prev page or spread (webtoon: one viewport; 2j for two)
  J / K          next / prev page or spread (webtoon: half a viewport)
  g / G          first / last page     (42G → page 42)
  f              open active book in native macOS fullscreen (f returns)
  z              toggle terminal focus mode (hide titles and status bar)
  t              toggle continuous webtoon mode (single pane)
  w              switch pane
  v              toggle split          (keeps the active pane)
  s              toggle two-page spread (single pane)
  tab            chapter menu          (book headings, ComicInfo.xml or folders)
  b              toggle bookmark on this page
  F              bookmarks menu
  S              screenshot page → PNG (YOYOMI_SHOT_DIR or cwd)
  R              rotate 90° cw
  i              toggle inversion (books: text pages; comics, PDF, DJVU: whole page)
  + / -          zoom in / out
  0              reset zoom
  arrows         pan while zoomed · scroll plain-text pages
  /              search: book text, OCR (tesseract) for images · n / p next / prev hit
  mouse wheel    next / prev page (webtoon: scroll)
  mouse          drag selects and copies text on book and PDF pages · click follows a link,
                 hovering shows where it goes, right click copies it without tracking parameters
  o / O          open file in pane / in split
  x              close pane
  e              open current book in browser (127.0.0.1:5xxxx)
  r              re-render
  ?              this help · any key to close
  q / ctrl+c     clear saved positions and quit
  Q              save reading positions and quit";

/// Begins and ends a synchronized update: the terminal shows the frame whole.
const BEGIN_UPDATE: &[u8] = b"\x1b[?2026h";
const END_UPDATE: &[u8] = b"\x1b[?2026l";
/// A tmux passthrough string with nothing inside, which tmux takes without
/// writing a byte to the terminal.
const EMPTY_PASSTHROUGH: &[u8] = b"\x1bPtmux;\x1b\\";

/// A sixel image to draw at screen cell (`x`, `y`) once the cells are out.
pub struct Paint {
    x: u16,
    y: u16,
    data: Arc<[u8]>,
}

/// Writes one frame inside a synchronized update: the kitty uploads of the
/// images it draws and the clipboard copy, then ratatui's diff of the cells,
/// then the sixel images, each drawn with the cursor saved and put back. The
/// other uploads, of frames ready ahead, follow the update, so the terminal
/// shows the frame before it parses them.
pub fn write_frame<W: Write>(terminal: &mut Terminal<CrosstermBackend<W>>, app: &mut App) -> io::Result<()> {
    let drawn = drawn_images(app);
    let (now, ahead): (Vec<Upload>, Vec<Upload>) = app.uploads.drain(..).partition(|upload| drawn.contains(&upload.id));
    let anew = drawn_anew(app, &drawn);
    let mut update = SyncUpdate { terminal, ended: false };
    let out = update.terminal.backend_mut();
    // tmux throws away its output for a terminal that reads slower than the
    // frames arrive, passthrough included, until the reader goes quiet, and
    // then redraws the cells it keeps; an upload in that window is lost.
    // After a passthrough string tmux keeps its output until the terminal
    // took it, so every frame starts with an empty one.
    if app.kitty_in_tmux() {
        out.write_all(EMPTY_PASSTHROUGH)?;
    }
    out.write_all(BEGIN_UPDATE)?;
    for upload in anew.iter().filter(|upload| !now.iter().any(|sent| same_upload(sent, upload))).chain(&now) {
        out.write_all(&upload.bytes)?;
    }
    if let Some(clipboard) = app.take_clipboard() {
        out.write_all(&clipboard)?;
    }
    let mut paints = Vec::new();
    update.terminal.draw(|frame| paints = draw(app, frame))?;
    app.sixel_fresh = false;
    let out = update.terminal.backend_mut();
    for paint in paints {
        write!(out, "\x1b7\x1b[{};{}H", paint.y + 1, paint.x + 1)?;
        out.write_all(&paint.data)?;
        out.write_all(b"\x1b8")?;
    }
    update.end()?;
    let out = update.terminal.backend_mut();
    for upload in ahead {
        out.write_all(&upload.bytes)?;
    }
    out.flush()
}

/// The kitty images of the scenes `draw` draws.
fn drawn_images(app: &App) -> Vec<ImageId> {
    if app.help || app.pick_for.is_some() || app.menu.is_some() {
        return Vec::new();
    }
    let scenes = (0..app.pane_count()).filter(|&i| app.body_errors(i).is_empty()).filter_map(|i| app.shown(i));
    scenes.flat_map(|shown| kitty_ids(&shown.scene)).collect()
}

/// The kitty images `scene` draws.
fn kitty_ids(scene: &Scene) -> impl Iterator<Item = ImageId> + '_ {
    scene.layers.iter().filter_map(|layer| if let Layer::KittyRows { id, .. } = layer { Some(*id) } else { None })
}

/// Under tmux, the uploads of the `drawn` images the previous frame did not
/// draw, sent again with the frame that draws them: tmux drops passthrough
/// that arrives while it redraws the window, and the next frame that draws
/// the image's cells anew delivers it.
fn drawn_anew(app: &mut App, drawn: &[ImageId]) -> Vec<Upload> {
    let on_screen: Vec<Upload> =
        (0..app.pane_count()).flat_map(|i| app.shown_uploads(i)).filter(|upload| drawn.contains(&upload.id)).cloned().collect();
    let anew = on_screen.iter().filter(|upload| !app.drawn.iter().any(|before| same_upload(before, upload))).cloned().collect();
    app.drawn = on_screen;
    anew
}

/// Whether two uploads carry the same image. An id that went to another
/// image comes with other bytes.
fn same_upload(a: &Upload, b: &Upload) -> bool {
    a.id == b.id && Arc::ptr_eq(&a.bytes, &b.bytes)
}

/// A synchronized update in progress. Dropped before `end`, by an error or a
/// panic, it still ends the update, so the terminal never holds a frame.
struct SyncUpdate<'a, W: Write> {
    terminal: &'a mut Terminal<CrosstermBackend<W>>,
    ended: bool,
}

impl<W: Write> SyncUpdate<'_, W> {
    fn end(&mut self) -> io::Result<()> {
        self.ended = true;
        let out = self.terminal.backend_mut();
        out.write_all(END_UPDATE)?;
        out.flush()
    }
}

impl<W: Write> Drop for SyncUpdate<'_, W> {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.end();
        }
    }
}

/// Draws the reader into `frame` and returns the sixel images to paint after
/// it.
pub fn draw(app: &App, frame: &mut Frame) -> Vec<Paint> {
    let area = frame.area();
    let buf = frame.buffer_mut();
    if area.is_empty() {
        return Vec::new();
    }
    if app.help {
        help(buf, app.help_page);
        return Vec::new();
    }
    if let Some(pane) = app.pick_for {
        app.picker.draw(buf, area, pane);
        return Vec::new();
    }
    if let Some(menu) = &app.menu {
        menu.draw(buf, area);
        return Vec::new();
    }
    let (body_top, body_rows) = if app.zen { (0, area.height) } else { (1, area.height.saturating_sub(2)) };
    let dim = Style::new().add_modifier(Modifier::DIM);
    if !app.zen || app.search_line.is_some() {
        status(buf, app, area.height.saturating_sub(1), area.width);
    }
    let middle = body_top + body_rows.saturating_sub(1) / 2;
    let mut paints = Vec::new();
    for i in 0..app.pane_count() {
        let columns = app.pane_columns(i);
        if !app.zen {
            let title = if i == app.active { Style::new().add_modifier(Modifier::BOLD).fg(Color::Indexed(212)) } else { dim };
            center(buf, columns, 0, &app.title(i), title);
        }
        let error = Style::new().fg(Color::Indexed(203));
        let errors = app.body_errors(i);
        let spread_errors = app.spread_errors(i);
        let image = app.scene_box(i);
        match app.shown(i) {
            Some(shown) if !spread_errors.is_empty() => {
                paints.extend(shown.scene.layers.iter().filter_map(|layer| draw_layer(buf, image, layer, app.sixel_fresh)));
                for (half, text) in spread_errors {
                    center(buf, half, middle, &text, error);
                }
            }
            _ if !errors.is_empty() => center(buf, columns, middle, &errors.join("; "), error),
            Some(shown) if shown.scene.layers.iter().any(|layer| image.shows(layer, area.bottom())) => {
                paints.extend(shown.scene.layers.iter().filter_map(|layer| draw_layer(buf, image, layer, app.sixel_fresh)));
            }
            _ if app.empty(i) => center(buf, columns, middle, "no book", dim),
            // A spread shows its loading text in its left half.
            _ => center(buf, app.spread_halves(i).map_or(columns, |[left, _]| left), middle, "loading…", dim),
        }
    }
    // The separator between split panes, beside the titles and bodies.
    if app.split {
        let x = app.pane_columns(1).0 - 1;
        for y in 0..(body_top + body_rows).min(area.height) {
            buf.set_stringn(x, y, "│", 1, dim);
        }
    }
    paints
}

/// How many pages the help takes on a screen of `rows` rows: one when it
/// fits, else as many as it fills above a last row that says which shows.
pub(crate) fn help_pages(rows: u16) -> usize {
    let lines = HELP.lines().count();
    if lines <= usize::from(rows) { 1 } else { lines.div_ceil(usize::from(rows.saturating_sub(1)).max(1)) }
}

/// Draws the help, centered, or page `page` of it with the page number in the
/// last row when it has more lines than the screen has rows.
fn help(buf: &mut Buffer, page: usize) {
    let area = buf.area;
    let lines: Vec<&str> = HELP.lines().collect();
    let widest = lines.iter().map(|line| width(line)).max().unwrap_or(0);
    let x = area.width.saturating_sub(widest as u16) / 2;
    let pages = help_pages(area.height);
    let (lines, rows) = if pages == 1 {
        (&lines[..], area.height)
    } else {
        let rows = area.height.saturating_sub(1).max(1);
        let page = page.min(pages - 1);
        let next = if page + 1 < pages { "any key for the next" } else { "any key to close" };
        let note = format!("page {} of {pages} · {next}", page + 1);
        buf.set_stringn(x, area.height - 1, note, usize::from(area.width), Style::new().add_modifier(Modifier::DIM));
        let per = usize::from(rows);
        (&lines[page * per..lines.len().min((page + 1) * per)], rows)
    };
    let y = rows.saturating_sub(lines.len() as u16) / 2;
    for (row, line) in (y..rows).zip(lines) {
        buf.set_stringn(x, row, line, usize::from(area.width), Style::new());
    }
}

/// Draws `layer` into `image`, the box its scene is placed in. A sixel image
/// draws after the cells: in the frame that paints it, its cells are written
/// blank to clear what was there; later frames skip them, so the image stays.
/// Ratatui's diff compares the diff option too, so once no image covers a
/// cell, it is written again over the old pixels. A sixel image that
/// `ImageBox::shows` refuses draws nothing.
fn draw_layer(buf: &mut Buffer, image: ImageBox, layer: &Layer, sixel_fresh: bool) -> Option<Paint> {
    let color = |Rgb(r, g, b): Rgb| Color::Rgb(r, g, b);
    match layer {
        Layer::KittyRows { id, cols, src_row, rows, at } => {
            let rows = (*rows).min(MAX_CELLS.saturating_sub(*src_row));
            each_cell(buf, image, *at, Grid::new((*cols).min(MAX_CELLS), rows), |col, row, cell| {
                cell.reset();
                cell.set_symbol(&String::from_iter(placeholder(src_row + row, col)))
                    .set_fg(color(id.color()))
                    .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
            });
        }
        Layer::Sixel { data, grid, at } => {
            if !image.shows(layer, buf.area.bottom()) {
                return None;
            }
            let skip = if sixel_fresh { CellDiffOption::AlwaysUpdate } else { CellDiffOption::Skip };
            each_cell(buf, image, *at, *grid, |_, _, cell| {
                cell.reset();
                cell.set_diff_option(skip);
            });
            return sixel_fresh.then(|| Paint { x: image.x + at.col, y: image.y + at.row, data: data.clone() });
        }
        Layer::Cells { grid, colors, at } => {
            each_cell(buf, image, *at, *grid, |col, row, cell| {
                if let Some([top, bottom]) = colors.get(usize::from(row) * usize::from(grid.cols) + usize::from(col)) {
                    cell.reset();
                    cell.set_char('▀').set_fg(color(*top)).set_bg(color(*bottom));
                }
            });
        }
        Layer::Text { lines, at } => {
            let (x, bottom) = (image.x + at.col, (image.y + image.rows).min(buf.area.bottom()));
            if x >= buf.area.right() {
                return None;
            }
            for (row, line) in (image.y + at.row..bottom).zip(lines.iter()) {
                buf.set_stringn(image.x + at.col, row, line, usize::from(image.cols.saturating_sub(at.col)), Style::new());
            }
        }
        _ => {}
    }
    None
}

/// Calls `paint` with each cell of `grid` placed at `at` in the image box,
/// cut to the box.
fn each_cell(buf: &mut Buffer, image: ImageBox, at: At, grid: Grid, mut paint: impl FnMut(u16, u16, &mut Cell)) {
    for row in 0..grid.rows.min(image.rows.saturating_sub(at.row)) {
        for col in 0..grid.cols.min(image.cols.saturating_sub(at.col)) {
            if let Some(cell) = buf.cell_mut((image.x + at.col + col, image.y + at.row + row)) {
                paint(col, row, cell);
            }
        }
    }
}

/// The status bar: what the reader is doing on the left, the tips on the
/// right, and the left part alone when both do not fit.
fn status(buf: &mut Buffer, app: &App, y: u16, cols: u16) {
    if let Some(input) = &app.search_line {
        let (x, _) = buf.set_stringn(0, y, format!(" /{}▏", safe_text(input)), usize::from(cols), search::INPUT_STYLE);
        buf.set_stringn(x, y, search::LINE_HINT, usize::from(cols.saturating_sub(x)), Style::new().add_modifier(Modifier::DIM));
        return;
    }
    let left = safe_text(&app.status_left());
    let gap = usize::from(cols).checked_sub(width(&left) + width(TIPS)).filter(|&gap| gap >= 1);
    let line = match gap {
        Some(gap) => format!("{left}{}{TIPS}", " ".repeat(gap)),
        None => truncate(&left, cols),
    };
    buf.set_stringn(0, y, line, usize::from(cols), Style::new().add_modifier(Modifier::DIM));
}

/// Draws `text` centered on row `y` of the `cols` columns from `x`, cut to
/// them with an ellipsis.
fn center(buf: &mut Buffer, (x, cols): (u16, u16), y: u16, text: &str, style: Style) {
    if y >= buf.area.bottom() || x >= buf.area.right() {
        return;
    }
    let text = truncate(&safe_text(text), cols);
    let x = x + cols.saturating_sub(width(&text) as u16) / 2;
    buf.set_stringn(x, y, text, usize::from(cols), style);
}

/// `text` without terminal escape sequences and then without control
/// characters, so a file name cannot retitle or recolor the terminal. ESC and
/// a byte from `@` to `_` is the 7-bit form of a C1 control: CSI runs to its
/// final byte; DCS, SOS, OSC, PM and APC run to BEL or ST.
pub(crate) fn safe_text(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(mut c) = chars.next() {
        if c == '\u{1b}' {
            let mut ahead = chars.clone();
            if let Some(next @ '@'..='_') = ahead.next() {
                chars = ahead;
                c = char::from_u32(u32::from(next) + 0x40).unwrap_or(c);
            }
        }
        match c {
            '\u{9b}' => {
                chars.by_ref().find(|c| ('@'..='~').contains(c));
            }
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => loop {
                let mut ahead = chars.clone();
                match ahead.next() {
                    None => break,
                    Some('\u{7}' | '\u{9c}') => {
                        chars = ahead;
                        break;
                    }
                    // ESC ends the string; it is ST when `\` follows, else the next sequence.
                    Some('\u{1b}') => {
                        if ahead.next() == Some('\\') {
                            chars = ahead;
                        }
                        break;
                    }
                    Some(_) => chars = ahead,
                }
            },
            // Other escape sequences: intermediates from space to `/`, then one final.
            '\u{1b}' => {
                chars.by_ref().find(|c| !(' '..='/').contains(c));
            }
            c if !c.is_control() => kept.push(c),
            _ => {}
        }
    }
    kept
}

pub(crate) fn width(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text` cut to `cols` cells, ending in an ellipsis when cut.
pub(crate) fn truncate(text: &str, cols: u16) -> String {
    let cols = usize::from(cols);
    if width(text) <= cols {
        return text.to_owned();
    }
    let mut cut = String::new();
    for c in text.chars() {
        let mut next = cut.clone();
        next.push(c);
        if width(&next) + 1 > cols {
            break;
        }
        cut = next;
    }
    cut.push('…');
    cut
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use crossterm::event::{Event as Input, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::{TerminalOptions, Viewport};
    use yoyomi_core::CellSize;
    use yoyomi_core::ImageIds;
    use yoyomi_render::{HalfBlock, Kitty, Sixel};
    use yoyomi_state::Positions;
    use yoyomi_view::{Resident, Shown, Update};

    use super::*;
    use crate::app::Event;
    use crate::app::tests::{app_with, deliver, fake, keys, reader, shown};
    use crate::select::tests::TextBook;

    fn open(app: &mut App) {
        deliver(app, 0, "/books/fake.cbz", Ok(fake(2, "/books/fake.cbz")));
    }

    /// A writer that keeps what the terminal would receive.
    #[derive(Clone, Default)]
    pub(crate) struct Recorder(Rc<RefCell<Vec<u8>>>);

    impl Recorder {
        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.borrow()).into_owned()
        }
    }

    impl Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A terminal that takes bytes until a write holds `<fail>`.
    struct FailingAt(Recorder);

    impl Write for FailingAt {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.windows(6).any(|window| window == b"<fail>") {
                return Err(io::Error::other("terminal gone"));
            }
            self.0.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    pub(crate) fn render(app: &App) -> (Buffer, usize) {
        let mut terminal = Terminal::new(TestBackend::new(app.cols, app.rows)).unwrap();
        let mut paints = 0;
        let frame = terminal.draw(|frame| paints = draw(app, frame).len()).unwrap();
        (frame.buffer.clone(), paints)
    }

    #[test]
    fn the_status_bar_keeps_the_tips_verbatim() {
        let mut app = reader(3);
        app.cols = 200;
        let (buf, _) = render(&app);
        let status = row(&buf, 23);
        assert!(status.starts_with(" halfblock "));
        assert!(status.ends_with("j/k page e in-browser s spread R rotate tab chapters  ? help  q quit "));
        app.cols = 40;
        let dir = tempfile::tempdir().unwrap();
        app.bookmarks = yoyomi_state::Bookmarks::load_from(Some(dir.path().join("bookmarks.json")));
        keys(&mut app, "b");
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 23).trim_end(), " halfblock  ·  bookmarked Fake p.1");
    }

    #[test]
    fn the_title_centers_and_cuts() {
        let mut app = reader(40);
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0).trim(), "Fake  1/40 …");
        assert_eq!(row(&buf, 11).trim(), "loading…");
        app.cols = 8;
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0), "Fake  1…");
    }

    #[test]
    fn help_shows_in_pages_when_longer_than_the_screen() {
        let mut app = reader(2);
        app.rows = 40;
        keys(&mut app, "?");
        let (buf, _) = render(&app);
        let rows: Vec<String> = (0..40).map(|y| row(&buf, y).trim().to_owned()).collect();
        assert_eq!(rows[4], "yy: terminal reader (.cbz/.cbr/.epub/.fb2/.docx/.doc/.pdf/.djvu)", "{rows:#?}");
        assert!(rows[35].starts_with("Q              save reading positions"), "{rows:#?}");
        assert!(!rows.iter().any(|row| row.starts_with("page ")));

        app.rows = 24;
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0).trim(), "yy: terminal reader (.cbz/.cbr/.epub/.fb2/.docx/.doc/.pdf/.djvu)");
        assert_eq!(row(&buf, 23).trim(), "page 1 of 2 · any key for the next");
        keys(&mut app, " ");
        let (buf, _) = render(&app);
        let rows: Vec<String> = (0..24).map(|y| row(&buf, y).trim().to_owned()).collect();
        assert!(rows[..23].iter().any(|row| row.starts_with("Q              save reading positions")), "{rows:#?}");
        assert_eq!(rows[23], "page 2 of 2 · any key to close");
    }

    #[test]
    fn safe_text_drops_escapes_and_controls() {
        assert_eq!(safe_text("title\x1b]2;spoof\x07\x1b[31mred\x1b[0m\n\t"), "titlered");
        assert_eq!(safe_text("a\x1b]2;x\x1b\\b\x1b(Bc\x1b7d\u{9b}1;2He"), "abcde");
        assert_eq!(safe_text("\x1b]2;x"), "");
        assert_eq!(safe_text("\x1bPdata\x1b[1mbold"), "bold");
    }

    #[test]
    fn the_title_and_status_show_no_escapes() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        deliver(&mut app, 0, "/nonexistent/\x1b[2Jbook\x1b]2;x.cbz", Err("open: \x1b[31mfailed\x07".into()));
        app.on_event(Event::Shot(Err("\x1b]0;owned\x07disk full".into())));
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0).trim(), "book");
        assert_eq!(row(&buf, 11).trim(), "open: failed");
        assert!(row(&buf, 23).starts_with(" halfblock  ·  screenshot failed: disk full "), "{:?}", row(&buf, 23));
    }

    #[test]
    fn split_panes_draw_side_by_side() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.split = true;
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(2, "/books/a.cbz")));
        app.apply(shown(vec![Layer::Text { lines: vec!["left".to_owned()].into(), at: At::default() }], Vec::new()));
        keys(&mut app, "w");
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0)[..39].trim(), "Fake  1/2");
        assert_eq!(&row(&buf, 1)[..5], "left ");
        assert_eq!(row(&buf, 0).chars().skip(40).collect::<String>().trim(), "[empty]  press o to open");
        assert_eq!(row(&buf, 11).chars().skip(40).collect::<String>().trim(), "no book");
        assert!((0..23).all(|y| buf[(39, y)].symbol() == "│"), "the separator runs down to the status bar");
        assert_ne!(buf[(39, 23)].symbol(), "│");
        let title_style = |x| buf[(x, 0)].modifier;
        assert!(title_style(17).contains(Modifier::DIM) && title_style(55).contains(Modifier::BOLD), "the active pane's title stands out");
    }

    #[test]
    fn tiny_screens_draw_what_fits() {
        let mut app = reader(2);
        let text = Layer::Text { lines: vec!["page".to_owned(); 3].into(), at: At::default() };
        app.apply(shown(vec![text], Vec::new()));
        for (cols, rows) in [(1, 1), (2, 2), (3, 1)] {
            (app.cols, app.rows) = (cols, rows);
            render(&app);
        }
    }

    #[test]
    fn kitty_rows_draw_placeholders() {
        let (mut app, _) = app_with(Arc::new(Kitty::new(CellSize::FALLBACK, false)), 20, 8);
        open(&mut app);
        let id = ImageId(0x1203);
        app.apply(shown(vec![Layer::KittyRows { id, cols: 3, src_row: 5, rows: 2, at: At { col: 2, row: 1 } }], Vec::new()));
        let (buf, paints) = render(&app);
        assert_eq!(paints, 0);
        let cell = &buf[(2, 2)];
        assert_eq!(cell.symbol(), String::from_iter(placeholder(5, 0)));
        assert_eq!(cell.fg, Color::Rgb(0, 18, 3));
        assert_eq!(cell.diff_option, CellDiffOption::ForcedWidth(NonZeroU16::MIN));
        assert_eq!(buf[(4, 3)].symbol(), String::from_iter(placeholder(6, 2)));
        assert_eq!(buf[(5, 3)].symbol(), " ");
    }

    #[test]
    fn sixel_cells_clear_once_then_skip() {
        let (mut app, _) = app_with(Arc::new(Sixel::new(CellSize::FALLBACK, 256)), 20, 8);
        let layer = Layer::Sixel { data: b"\x1bPq#0~\x1b\\".as_slice().into(), grid: Grid::new(4, 2), at: At { col: 1, row: 0 } };
        open(&mut app);
        app.apply(shown(vec![layer], Vec::new()));
        let (buf, paints) = render(&app);
        assert_eq!(paints, 1);
        assert_eq!(buf[(2, 1)].diff_option, CellDiffOption::AlwaysUpdate);
        app.sixel_fresh = false;
        let (buf, paints) = render(&app);
        assert_eq!(paints, 0);
        assert_eq!(buf[(2, 1)].diff_option, CellDiffOption::Skip);
        assert_eq!(buf[(6, 1)].diff_option, CellDiffOption::None);
    }

    #[test]
    fn a_sixel_image_paints_and_takes_the_mouse_only_where_all_of_it_fits() {
        let (mut app, received) = app_with(Arc::new(Sixel::new(CellSize::FALLBACK, 256)), 20, 8);
        deliver(&mut app, 0, "/books/text.pdf", Ok(Arc::new(TextBook)));
        keys(&mut app, "z");
        // Focus mode keeps the bottom row free: the image fills the 7 rows above it.
        let layer = Layer::Sixel { data: b"<sixel>".as_slice().into(), grid: Grid::new(20, 7), at: At::default() };
        app.apply(shown(vec![layer], Vec::new()));
        let (buf, paints) = render(&app);
        assert_eq!(paints, 1);
        assert_eq!(buf[(0, 6)].diff_option, CellDiffOption::AlwaysUpdate);
        // Cell (2, 2) lies on "visit", a web link; cell (2, 3) on "next", a link to page 2.
        let mouse = |app: &mut App, kind, (column, row)| {
            app.on_event(Event::Input(Input::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })));
        };
        mouse(&mut app, MouseEventKind::Moved, (2, 2));
        while !app.status_left().contains('→') {
            app.on_event(received.recv_timeout(Duration::from_secs(5)).expect("the text layer loads"));
        }

        // Leaving focus mode shrinks the box; the old image waits for its
        // render, its cells go out as text over the old pixels, the body says
        // it loads, and the mouse finds nothing on it.
        keys(&mut app, "z");
        let (buf, paints) = render(&app);
        assert_eq!(paints, 0);
        assert_eq!(buf[(1, 2)].diff_option, CellDiffOption::None);
        assert_eq!(row(&buf, 3).trim(), "loading…");
        for y in 0..8 {
            mouse(&mut app, MouseEventKind::Moved, (2, y));
            assert!(!app.status_left().contains('→'), "row {y}: {}", app.status_left());
        }
        for y in [3, 4] {
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), (2, y));
            mouse(&mut app, MouseEventKind::Up(MouseButton::Left), (2, y));
        }
        assert!(app.title(0).starts_with("text  1/2"), "no link opened: {}", app.title(0));
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), (0, 1));
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), (19, 6));
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), (19, 6));
        assert_eq!(app.take_clipboard(), None, "nothing to select");

        // A whole-screen image in focus mode would reach the bottom row.
        keys(&mut app, "z");
        let layer = Layer::Sixel { data: b"<sixel>".as_slice().into(), grid: Grid::new(20, 8), at: At::default() };
        app.apply(shown(vec![layer], Vec::new()));
        assert_eq!(render(&app).1, 0);
        mouse(&mut app, MouseEventKind::Moved, (2, 2));
        assert!(!app.status_left().contains('→'), "{}", app.status_left());
    }

    #[test]
    fn half_blocks_and_text_draw_cells() {
        let mut app = reader(2);
        let colors = vec![[Rgb(1, 2, 3), Rgb(4, 5, 6)], [Rgb(7, 8, 9), Rgb(10, 11, 12)]];
        let cells = Layer::Cells { grid: Grid::new(2, 1), colors: colors.into(), at: At { col: 0, row: 0 } };
        let text = Layer::Text { lines: vec!["hello".to_owned()].into(), at: At { col: 0, row: 2 } };
        app.apply(shown(vec![cells, text], Vec::new()));
        let (buf, _) = render(&app);
        let cell = &buf[(1, 1)];
        assert_eq!((cell.symbol(), cell.fg, cell.bg), ("▀", Color::Rgb(7, 8, 9), Color::Rgb(10, 11, 12)));
        assert_eq!(&row(&buf, 3)[..5], "hello");
    }

    #[test]
    fn errors_replace_an_empty_scene() {
        let mut app = reader(2);
        app.apply(shown(Vec::new(), vec!["page 1: corrupt".into()]));
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 11).trim(), "page 1: corrupt");
    }

    #[test]
    fn a_failed_page_of_a_spread_shows_its_error_in_its_half() {
        let mut app = reader(3);
        keys(&mut app, "s");
        assert_eq!(row(&render(&app).0, 11)[..40].trim(), "loading…", "a spread loads in its left half");
        let left = |text: &str| Layer::Text { lines: vec![text.to_owned()].into(), at: At::default() };
        app.apply(shown(vec![left("left")], vec!["page 2: corrupt".into()]));
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0).trim(), "Fake  1-2/3", "the title keeps to the pages");
        let middle = row(&buf, 11);
        assert_eq!(middle[..40].trim(), "");
        assert_eq!(middle[40..].trim(), "page 2: corrupt");

        // Both pages fail, also in focus mode, where no title shows.
        keys(&mut app, "z");
        app.apply(shown(Vec::new(), vec!["page 1: corrupt".into(), "page 2: gone".into()]));
        let (buf, _) = render(&app);
        let middle = row(&buf, 11);
        assert_eq!(middle[..40].trim(), "page 1: corrupt");
        assert_eq!(middle[40..].trim(), "page 2: gone");

        // The last page of a spread stands alone on the left.
        keys(&mut app, "j");
        app.apply(Update { shown: Some(Shown { page: 2, scene: Scene::default(), errors: vec!["page 3: corrupt".into()] }), ..Update::default() });
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 11)[..40].trim(), "page 3: corrupt");
    }

    #[test]
    fn focus_mode_hides_title_and_status() {
        let mut app = reader(2);
        keys(&mut app, "z");
        let text = Layer::Text { lines: vec!["top".to_owned()].into(), at: At::default() };
        app.apply(shown(vec![text], Vec::new()));
        let (buf, _) = render(&app);
        assert_eq!(row(&buf, 0).trim(), "top");
        assert_eq!(row(&buf, 23).trim(), "");
    }

    #[test]
    fn the_writer_orders_uploads_cells_and_sixel_paints() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 20, 6);
        open(&mut app);
        app.uploads.push(Upload { id: ImageId(8), bytes: b"<ahead>".as_slice().into() });
        app.uploads.push(Upload { id: ImageId(7), bytes: b"<upload>".as_slice().into() });
        let text = Layer::Text { lines: vec!["cells".to_owned()].into(), at: At::default() };
        let kitty = Layer::KittyRows { id: ImageId(7), cols: 1, src_row: 0, rows: 1, at: At { col: 0, row: 3 } };
        let sixel = Layer::Sixel { data: b"<sixel>".as_slice().into(), grid: Grid::new(2, 1), at: At { col: 4, row: 2 } };
        app.apply(shown(vec![text, kitty, sixel], Vec::new()));
        let options = TerminalOptions { viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 20, 6)) };
        let written = Recorder::default();
        let mut terminal = Terminal::with_options(CrosstermBackend::new(written.clone()), options).unwrap();
        write_frame(&mut terminal, &mut app).unwrap();
        let out = written.text();
        let at = |needle: &str| out.find(needle).unwrap_or_else(|| panic!("{needle:?} missing from {out:?}"));
        assert_eq!(at("\x1b[?2026h"), 0);
        assert!(at("<upload>") < at("cells"));
        assert!(at("cells") < at("\x1b7\x1b[4;5H<sixel>\x1b8"));
        // An upload of an image the frame does not draw waits until it shows.
        assert!(out.ends_with("\x1b8\x1b[?2026l<ahead>"), "{out:?}");
        assert!(app.uploads.is_empty() && !app.sixel_fresh);

        // The next frame sends no upload and leaves the image alone.
        write_frame(&mut terminal, &mut app).unwrap();
        let again = written.text()[out.len()..].to_owned();
        assert!(!again.contains("<upload>") && !again.contains("<ahead>") && !again.contains("<sixel>"), "{again:?}");
    }

    /// Writes a frame and returns its bytes.
    fn write(terminal: &mut Terminal<CrosstermBackend<Recorder>>, written: &Recorder, app: &mut App) -> String {
        let before = written.text().len();
        write_frame(terminal, app).unwrap();
        written.text()[before..].to_owned()
    }

    #[test]
    fn under_tmux_a_frame_sends_the_images_it_draws_anew() {
        for tmux in [true, false] {
            let (events, _) = crossbeam_channel::unbounded();
            let (view_events, _) = crossbeam_channel::unbounded();
            let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), tmux));
            let renderer = Arc::new(Kitty::new(CellSize::FALLBACK, false));
            let mut app = App::new(renderer, yoyomi_render::Terminal::default(), resident, Positions::load_from(None), events, view_events, (20, 6));
            open(&mut app);
            let kitty = Layer::KittyRows { id: ImageId(7), cols: 2, src_row: 0, rows: 1, at: At::default() };
            let upload = |bytes: &str| Upload { id: ImageId(7), bytes: bytes.as_bytes().into() };
            let with = |upload: &Upload| Update { uploads: vec![upload.clone()], ..shown(vec![kitty.clone()], Vec::new()) };
            let options = TerminalOptions { viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 20, 6)) };
            let written = Recorder::default();
            let mut terminal = Terminal::with_options(CrosstermBackend::new(written.clone()), options).unwrap();

            let one = upload("<one>");
            app.apply(with(&one));
            let out = write(&mut terminal, &written, &mut app);
            assert_eq!(out.starts_with("\x1bPtmux;\x1b\\\x1b[?2026h"), tmux, "{out:?}");
            assert_eq!(out.matches("<one>").count(), 1, "{out:?}");
            assert!(out.find("<one>") < out.find('\u{10EEEE}'), "the upload goes before the cells: {out:?}");
            assert!(!write(&mut terminal, &written, &mut app).contains("<one>"), "cells drawn again send nothing");

            // Help covers the page; the page comes back.
            app.help = true;
            let out = write(&mut terminal, &written, &mut app);
            assert!(!out.contains("<one>"));
            assert_eq!(out.starts_with("\x1bPtmux;\x1b\\"), tmux, "the help frame too: {out:?}");
            app.help = false;
            let out = write(&mut terminal, &written, &mut app);
            assert_eq!(out.matches("<one>").count(), usize::from(tmux), "tmux {tmux}: {out:?}");
            assert!(!tmux || out.find("\x1b[?2026h") < out.find("<one>") && out.find("<one>") < out.find('\u{10EEEE}'), "{out:?}");

            // The same frame again sends nothing until a menu covered it.
            // Outside tmux a view sends an upload once.
            app.apply(if tmux { with(&one) } else { shown(vec![kitty.clone()], Vec::new()) });
            assert!(!write(&mut terminal, &written, &mut app).contains("<one>"));
            app.menu = Some(crate::menus::Menu::bookmarks(&[]));
            write(&mut terminal, &written, &mut app);
            app.menu = None;
            assert_eq!(write(&mut terminal, &written, &mut app).matches("<one>").count(), usize::from(tmux));

            // Another image under the same id goes out once.
            app.apply(with(&upload("<two>")));
            assert_eq!(write(&mut terminal, &written, &mut app).matches("<two>").count(), 1);
            assert!(!write(&mut terminal, &written, &mut app).contains("<two>"));
        }

        // Under tmux other renderers send no passthrough.
        let (events, _) = crossbeam_channel::unbounded();
        let (view_events, _) = crossbeam_channel::unbounded();
        let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), true));
        let renderer = Arc::new(HalfBlock::new(CellSize::FALLBACK));
        let mut app = App::new(renderer, yoyomi_render::Terminal::default(), resident, Positions::load_from(None), events, view_events, (20, 6));
        let options = TerminalOptions { viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 20, 6)) };
        let written = Recorder::default();
        let mut terminal = Terminal::with_options(CrosstermBackend::new(written.clone()), options).unwrap();
        assert!(write(&mut terminal, &written, &mut app).starts_with("\x1b[?2026h"));
    }

    #[test]
    fn a_smaller_sixel_image_rewrites_the_cells_it_no_longer_covers() {
        let (mut app, _) = app_with(Arc::new(Sixel::new(CellSize::FALLBACK, 256)), 20, 8);
        open(&mut app);
        let sixel = |data: &str, grid, col| Layer::Sixel { data: data.as_bytes().into(), grid, at: At { col, row: 0 } };
        app.apply(shown(vec![sixel("<left>", Grid::new(4, 2), 0), sixel("<right>", Grid::new(4, 2), 4)], Vec::new()));
        let options = TerminalOptions { viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 20, 8)) };
        let written = Recorder::default();
        let mut terminal = Terminal::with_options(CrosstermBackend::new(written.clone()), options).unwrap();
        write_frame(&mut terminal, &mut app).unwrap();
        // A key that redraws, such as `t`, paints the spread once more.
        app.sixel_fresh = true;
        write_frame(&mut terminal, &mut app).unwrap();

        // One 3x1 image replaces the spread: both rows of the old 8x2
        // rectangle go out blank before the new image paints.
        let before = written.text().len();
        app.apply(shown(vec![sixel("<one>", Grid::new(3, 1), 0)], Vec::new()));
        write_frame(&mut terminal, &mut app).unwrap();
        let out = written.text()[before..].to_owned();
        assert!(out.contains("\x1b[2;1H        ") && out.contains("\x1b[3;1H        "), "{out:?}");
        assert!(out.contains("\x1b[2;1H<one>"), "{out:?}");

        // Once no image is left, its cells go out blank as well.
        let before = written.text().len();
        let text = Layer::Text { lines: vec!["x".to_owned()].into(), at: At { col: 8, row: 0 } };
        app.apply(shown(vec![text], Vec::new()));
        write_frame(&mut terminal, &mut app).unwrap();
        let out = written.text()[before..].to_owned();
        assert!(out.contains("\x1b[2;1H   "), "{out:?}");
    }

    #[test]
    fn a_failed_frame_still_ends_the_update() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 20, 6);
        open(&mut app);
        let text = Layer::Text { lines: vec!["cells".to_owned()].into(), at: At::default() };
        let sixel = Layer::Sixel { data: b"<fail>".as_slice().into(), grid: Grid::new(2, 1), at: At { col: 4, row: 2 } };
        app.apply(shown(vec![text, sixel], Vec::new()));
        let options = TerminalOptions { viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 20, 6)) };
        let written = Recorder::default();
        let mut terminal = Terminal::with_options(CrosstermBackend::new(FailingAt(written.clone())), options).unwrap();
        assert!(write_frame(&mut terminal, &mut app).is_err());
        let out = written.text();
        assert!(out.starts_with("\x1b[?2026h") && out.contains("cells"), "{out:?}");
        assert!(out.ends_with("\x1b[?2026l"), "{out:?}");
    }
}
