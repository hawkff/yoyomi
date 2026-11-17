//! Page turns. The pager shows the requested page or spread and readies the
//! next one, so a turn to it shows a frame the terminal already holds.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Sender;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use yoyomi_core::{
    At, Book, Cancel, Frame, Grid, ImageId, Jobs, Layer, PageCache, PageKind, Priority, Renderer, RendererKind, Result, Transform, Upload,
};

use crate::{Lease, Resident, Shown, Update, ViewEvent, next_source};

#[cfg(test)]
mod tests;

/// How many frames a pager keeps: the shown spread and the ones on either
/// side, with room to spare.
const MAX_FRAMES: usize = 8;
/// How many frames a native pager keeps: the shown spread and the next one.
/// Its frames are full-size images, about 20 MB each on a Retina screen.
const MAX_NATIVE_FRAMES: usize = 4;

/// The widest a plain-text page gets, for readability.
const MAX_TEXT_COLS: usize = 72;

/// What a pane wants to show.
#[derive(Clone, Debug, PartialEq)]
pub struct PageRequest {
    pub page: usize,
    /// Show `page` and `page + 1` side by side.
    pub spread: bool,
    /// The body the pages are placed in: the pane below its title. A spread
    /// halves it into one slot per page, and each page sits centered in its
    /// slot.
    pub grid: Grid,
    /// The columns and rows that come off each slot before a page is fitted
    /// into it.
    pub margin: Grid,
    pub transform: Transform,
    /// The page `transform.highlights` belong to; the other pages render
    /// without them.
    pub selected_page: usize,
    /// Rows a plain-text page scrolls down; see `Pager::text_rows`.
    pub text_top: usize,
}

/// Shows the pages of one book in one pane.
pub struct Pager {
    source: u64,
    book: Arc<PageCache>,
    renderer: Arc<dyn Renderer>,
    jobs: Jobs,
    resident: Arc<Resident>,
    events: Sender<ViewEvent>,
    request: Option<PageRequest>,
    /// Whether the frame of `request` went out.
    delivered: bool,
    /// The pages of `request` that failed, with their messages.
    failed: HashMap<FrameKey, String>,
    frames: Vec<Cached>,
    /// The jobs whose results still count, by the frame they render.
    running: HashMap<FrameKey, Running>,
    next_job: u64,
    /// The clock that orders `frames` by last use.
    tick: u64,
}

/// A job whose result still counts.
struct Running {
    job: u64,
    cancel: Cancel,
    priority: Priority,
    /// Set by the job once a worker runs it.
    started: Arc<AtomicBool>,
}

/// Everything a frame depends on. One pager serves one book, so the book is
/// not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FrameKey {
    page: usize,
    /// The box the page fits into.
    max: Grid,
    /// The cell size, bit for bit.
    cell: (u64, u64),
    /// The transform, with `invert` off where the book cannot invert the page.
    transform: Transform,
    kind: RendererKind,
}

struct Cached {
    key: FrameKey,
    /// The frame, without kitty upload bytes.
    frame: Frame,
    lease: Option<Lease>,
    /// Under tmux, the upload, which goes out with every show of the frame.
    upload: Option<Upload>,
    used: u64,
}

/// Where one page of a request goes: its frame and the columns and rows of
/// its slot.
struct Slot {
    key: FrameKey,
    col: u16,
    cols: u16,
    rows: u16,
}

impl Slot {
    /// Where a frame of `grid` sits centered in the slot.
    fn at(&self, grid: Grid) -> At {
        At { col: self.col + self.cols.saturating_sub(grid.cols) / 2, row: self.rows.saturating_sub(grid.rows) / 2 }
    }
}

/// A finished render job.
struct Rendered {
    job: u64,
    key: FrameKey,
    result: Result<Frame>,
    claim: Claim,
}

/// The kitty id a render job took. Dropping the claim gives the id back, so
/// a result that no pager keeps, such as one that outlived its pager, holds
/// no id.
struct Claim {
    lease: Option<Lease>,
    resident: Arc<Resident>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.resident.release(lease);
        }
    }
}

impl Pager {
    /// A pager for `book`. Its jobs run on `jobs` and report through
    /// `events`; kitty frames take their ids from `resident`.
    pub fn new(book: Arc<PageCache>, renderer: Arc<dyn Renderer>, jobs: Jobs, resident: Arc<Resident>, events: Sender<ViewEvent>) -> Self {
        Pager {
            source: next_source(),
            book,
            renderer,
            jobs,
            resident,
            events,
            request: None,
            delivered: false,
            failed: HashMap::new(),
            frames: Vec::new(),
            running: HashMap::new(),
            next_job: 0,
            tick: 0,
        }
    }

    /// Asks for `request`. A frame that is ready comes back at once;
    /// otherwise a job starts and its frame arrives through `on_event`. A
    /// newer request cancels older work, and the pages after the shown ones
    /// render in the background.
    pub fn show(&mut self, request: PageRequest) -> Update {
        let shown: Vec<FrameKey> = self.slots(&request).into_iter().map(|slot| slot.key).collect();
        let ahead: Vec<FrameKey> = self.neighbors(&request).into_iter().flatten().map(|slot| slot.key).collect();
        // Neighbors still in the way of the new request keep rendering. A
        // neighbor that now shows but still waits in the queue gives way to a
        // visible job.
        self.running.retain(|key, running| {
            let keep = if shown.contains(key) {
                running.priority == Priority::Visible || running.started.load(Ordering::SeqCst)
            } else {
                running.priority == Priority::Neighbor && ahead.contains(key)
            };
            if !keep {
                running.cancel.cancel();
            }
            keep
        });
        self.request = Some(request);
        self.delivered = false;
        self.failed.clear();
        let mut update = Update::default();
        self.advance(&mut update);
        update
    }

    /// Takes a finished job of this pager, or hands back an event it did not
    /// start.
    pub fn on_event(&mut self, event: ViewEvent) -> Result<Update, ViewEvent> {
        let Rendered { job, key, result, mut claim } = event.take::<Rendered>(self.source)?;
        let mut update = Update::default();
        let priority = match self.running.get(&key) {
            Some(running) if running.job == job => running.priority,
            _ => return Ok(update),
        };
        self.running.remove(&key);
        match result {
            // Another frame took the id while this one rendered; its upload
            // would replace that frame's image.
            Ok(_) if claim.lease.is_some_and(|lease| !self.resident.holds(lease)) => self.start(key, priority),
            Ok(frame) => {
                let lease = claim.lease.take();
                let upload = Upload::from_frame(&frame);
                // Under tmux the upload goes out with every show of the frame.
                let (kept, ahead) = if self.resident.resend() { (upload, None) } else { (None, upload) };
                update.uploads.extend(ahead);
                let frame = match frame {
                    Frame::Kitty { id, grid, .. } => Frame::Kitty { id, grid, upload: Arc::from([]) },
                    other => other,
                };
                self.insert(Cached { key, frame, lease, upload: kept, used: 0 });
            }
            // A neighbor that fails renders again, and reports, once it shows.
            Err(error) => {
                if self.request.as_ref().is_some_and(|request| self.slots(request).iter().any(|slot| slot.key == key)) {
                    self.failed.insert(key.clone(), format!("page {}: {error}", key.page + 1));
                }
            }
        }
        self.advance(&mut update);
        Ok(update)
    }

    /// Drops every frame the pager keeps and gives their ids back to the
    /// `Resident`.
    pub fn clear(&mut self) {
        for (_, running) in self.running.drain() {
            running.cancel.cancel();
        }
        for cached in self.frames.drain(..) {
            if let Some(lease) = cached.lease {
                self.resident.release(lease);
            }
        }
        self.request = None;
        self.failed.clear();
    }

    /// The slots of `request`: one page fills the body; in a spread the left
    /// slot takes half the columns but the gap, the right slot the rest after
    /// the gap, and both pages fit the left slot's width. Each page fits its
    /// slot less the margin. The gap is one cell, or for the pixels of the
    /// native window a 300th of the width within 2 to 8. A spread on the last
    /// page shows that page on the left.
    fn slots(&self, request: &PageRequest) -> Vec<Slot> {
        let Grid { cols, rows } = request.grid;
        let fit = |cols: u16| Grid::new(cols.saturating_sub(request.margin.cols).max(1), rows.saturating_sub(request.margin.rows).max(1));
        let cell = self.renderer.cell_size();
        let key = |page: usize, max: Grid| FrameKey {
            page,
            max,
            cell: (cell.w.to_bits(), cell.h.to_bits()),
            transform: Transform {
                invert: request.transform.invert && self.book.can_invert(page),
                highlights: if page == request.selected_page { request.transform.highlights.clone() } else { Arc::from([]) },
                ..request.transform.clone()
            },
            kind: self.renderer.kind(),
        };
        if !request.spread {
            return vec![Slot { key: key(request.page, fit(cols)), col: 0, cols, rows }];
        }
        let gap = if self.renderer.kind() == RendererKind::Native { (cols / 300).clamp(2, 8) } else { 1 };
        let half = cols.saturating_sub(gap) / 2;
        let max = fit(half);
        let mut slots = vec![Slot { key: key(request.page, max), col: 0, cols: half, rows }];
        if request.page + 1 < self.book.len() {
            slots.push(Slot { key: key(request.page + 1, max), col: half + gap, cols: cols.saturating_sub(half + gap), rows });
        }
        slots
    }

    /// Where `frames` holds the frame for `key`.
    fn find(&self, key: &FrameKey) -> Option<usize> {
        self.frames.iter().position(|cached| cached.key == *key)
    }

    /// Forgets frames whose kitty id went to another user; they render again.
    fn drop_lost(&mut self) {
        let resident = &self.resident;
        self.frames.retain(|cached| cached.lease.is_none_or(|lease| resident.holds(lease)));
    }

    /// How many frames the pager keeps.
    fn max_frames(&self) -> usize {
        if self.renderer.kind() == RendererKind::Native { MAX_NATIVE_FRAMES } else { MAX_FRAMES }
    }

    /// Keeps `cached` as the most recent frame and evicts the least recently
    /// used ones past `max_frames`, releasing their kitty ids. The frames of
    /// the request and of the next page or spread go last.
    fn insert(&mut self, mut cached: Cached) {
        self.tick += 1;
        cached.used = self.tick;
        if let Some(i) = self.find(&cached.key) {
            self.release(i);
        }
        self.frames.push(cached);
        let kept: Vec<FrameKey> = self.request.as_ref().map_or_else(Vec::new, |request| {
            let next = self.neighbors(request).into_iter().next().unwrap_or_default();
            self.slots(request).into_iter().chain(next).map(|slot| slot.key).collect()
        });
        while self.frames.len() > self.max_frames() {
            let oldest =
                (0..self.frames.len()).min_by_key(|&i| (kept.contains(&self.frames[i].key), self.frames[i].used)).expect("frames is not empty");
            self.release(oldest);
        }
    }

    fn release(&mut self, i: usize) {
        if let Some(lease) = self.frames.swap_remove(i).lease {
            self.resident.release(lease);
        }
    }

    /// Delivers the frame of the latest request once all its pages are
    /// rendered or failed, then readies its neighbors. Until then, starts the
    /// renders it still lacks.
    fn advance(&mut self, update: &mut Update) {
        let Some(request) = self.request.clone() else { return };
        if self.delivered {
            return;
        }
        self.drop_lost();
        let slots = self.slots(&request);
        let found: Vec<Option<usize>> = slots.iter().map(|slot| self.find(&slot.key)).collect();
        let mut ready = true;
        for (slot, found) in slots.iter().zip(&found) {
            if found.is_none() && !self.failed.contains_key(&slot.key) && !self.is_text(slot.key.page) {
                ready = false;
                self.start(slot.key.clone(), Priority::Visible);
            }
        }
        if !ready {
            return;
        }
        self.delivered = true;
        let mut shown = Shown { page: request.page, ..Shown::default() };
        for (slot, found) in slots.iter().zip(found) {
            if self.is_text(slot.key.page) {
                let lines = text_rows(&self.book.page_text(slot.key.page).unwrap_or_default(), request.text_top, slot.key.max);
                let grid = Grid::new(lines.first().map_or(0, |line| cells(line)) as u16, lines.len() as u16);
                shown.scene.layers.push(Layer::Text { lines: lines.into(), at: slot.at(grid) });
                continue;
            }
            let Some(i) = found else {
                shown.errors.push(self.failed[&slot.key].clone());
                continue;
            };
            self.tick += 1;
            let cached = &mut self.frames[i];
            cached.used = self.tick;
            if let Some(lease) = cached.lease {
                self.resident.touch(lease);
            }
            update.uploads.extend(cached.upload.clone());
            shown.scene.layers.push(Layer::from_frame(&cached.frame, slot.at(cached.frame.grid())));
        }
        update.shown = Some(shown);
        let pages: Vec<usize> = slots.iter().map(|slot| slot.key.page).collect();
        self.ready_neighbors(&request, &pages);
    }

    /// Renders the next page or spread, then the previous one, unless their
    /// frames pass `max_frames` or their pages would push the shown ones out
    /// of the page cache. As in
    /// `PageCache::room_to_preload`, each set of neighbors counts as many
    /// pixels as the shown pages.
    fn ready_neighbors(&mut self, request: &PageRequest, shown: &[usize]) {
        let pixels: u64 =
            shown.iter().filter_map(|&page| self.book.cached(page)).map(|page| u64::from(page.width()) * u64::from(page.height())).sum();
        let (mut sets, mut frames) = (1, shown.len());
        for set in self.neighbors(request) {
            // A set past the frame limit would push out the shown frames or the next ones.
            frames += set.len();
            if frames > self.max_frames() {
                return;
            }
            let missing: Vec<FrameKey> =
                set.into_iter().map(|slot| slot.key).filter(|key| self.find(key).is_none() && !self.is_text(key.page)).collect();
            if missing.is_empty() {
                continue;
            }
            sets += 1;
            if sets * pixels > PageCache::MAX_PIXELS {
                return;
            }
            for key in missing {
                self.start(key, Priority::Neighbor);
            }
        }
    }

    /// The slots of the next page or spread, then of the previous one.
    fn neighbors(&self, request: &PageRequest) -> Vec<Vec<Slot>> {
        let mut sets = Vec::new();
        for delta in [1, -1] {
            let page = turn(request.page, delta, request.spread, self.book.len());
            if page != request.page {
                let transform = Transform { highlights: Arc::from([]), ..request.transform.clone() };
                sets.push(self.slots(&PageRequest { page, transform, ..request.clone() }));
            }
        }
        sets
    }

    /// Whether `page` shows as plain text: a text page under half blocks,
    /// which draw no pixels fine enough to read.
    fn is_text(&self, page: usize) -> bool {
        self.renderer.kind() == RendererKind::HalfBlock && self.book.kind(page) == PageKind::Text
    }

    /// How many rows the longest plain-text page of `request` wraps to, or
    /// 0 when it shows none; `PageRequest::text_top` scrolls within them.
    pub fn text_rows(&self, request: &PageRequest) -> usize {
        let slots = self.slots(request);
        let pages = slots.iter().filter(|slot| self.is_text(slot.key.page));
        pages.filter_map(|slot| Some(wrap(&self.book.page_text(slot.key.page)?, text_cols(slot.key.max)).len())).max().unwrap_or(0)
    }

    /// Starts a job that renders `key`, unless one already does.
    fn start(&mut self, key: FrameKey, priority: Priority) {
        if self.running.contains_key(&key) {
            return;
        }
        self.next_job += 1;
        let (job, cancel, started) = (self.next_job, Cancel::new(), Arc::new(AtomicBool::new(false)));
        self.running.insert(key.clone(), Running { job, cancel: cancel.clone(), priority, started: started.clone() });
        let (book, renderer, resident, events, source) =
            (self.book.clone(), self.renderer.clone(), self.resident.clone(), self.events.clone(), self.source);
        self.jobs.spawn(priority, &cancel.clone(), move || {
            started.store(true, Ordering::SeqCst);
            let mut claim = Claim { lease: None, resident };
            let result = render(&book, renderer.as_ref(), &mut claim, &key, &cancel);
            // A closed channel drops the event, and its claim gives the id back.
            let _ = events.send(ViewEvent::new(source, Rendered { job, key, result, claim }));
        });
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Decodes, transforms and renders one frame. Kitty frames take a lease
/// into `claim` right before rendering, so cancelled work holds no id.
fn render(book: &PageCache, renderer: &dyn Renderer, claim: &mut Claim, key: &FrameKey, cancel: &Cancel) -> Result<Frame> {
    let pixels = book.page(key.page, cancel)?;
    let (image, invert) = yoyomi_render::apply(&pixels, &key.transform);
    cancel.check()?;
    let lease = (renderer.kind() == RendererKind::Kitty).then(|| claim.resident.lease());
    claim.lease = lease;
    renderer.render(&image, lease.map_or(ImageId(0), |lease| lease.id), key.max, invert)
}

/// The page a turn by `delta` from `page` leads to: a spread moves two pages,
/// and past the end it stops on the last pair rather than a lone last page.
fn turn(page: usize, delta: isize, spread: bool, len: usize) -> usize {
    let last = len.saturating_sub(1) as isize;
    let page = page as isize;
    let step = if spread { 2 } else { 1 };
    let delta = if spread && page + delta * step > last { (last - page) / step } else { delta };
    (page + delta * step).clamp(0, last) as usize
}

fn text_cols(max: Grid) -> usize {
    usize::from(max.cols).clamp(1, MAX_TEXT_COLS)
}

/// The terminal cells `text` takes as ratatui draws it: grapheme by
/// grapheme, each measured as a string, so a sequence such as an emoji
/// joined with zero-width joiners counts once.
fn cells(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Where `text` passes `cols` terminal cells: the byte index of the first
/// grapheme that ends past them.
fn cut_at(text: &str, cols: usize) -> Option<usize> {
    let mut width = 0;
    text.grapheme_indices(true).find_map(|(i, grapheme)| {
        width += grapheme.width();
        (width > cols).then_some(i)
    })
}

/// Paragraphs wrapped at word boundaries into lines of at most `cols`
/// terminal cells. Longer words break between graphemes, keeping at least one
/// on a line.
fn wrap(paragraphs: &[String], cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in paragraphs {
        let (mut line, mut len) = (String::new(), 0);
        for word in paragraph.split(' ') {
            let mut word: String = word.chars().filter(|c| !c.is_control()).collect();
            loop {
                let width = cells(&word);
                if len + usize::from(len > 0) + width <= cols {
                    if len > 0 {
                        line.push(' ');
                        len += 1;
                    }
                    len += width;
                    line.push_str(&word);
                    break;
                }
                if len > 0 {
                    lines.push(std::mem::take(&mut line));
                    len = 0;
                    continue;
                }
                let first = word.graphemes(true).next().map_or(0, str::len);
                let cut = cut_at(&word, cols).unwrap_or(word.len()).max(first);
                if cut == word.len() {
                    // One grapheme wider than the line.
                    (line, len) = (word, width);
                    break;
                }
                lines.push(word.drain(..cut).collect());
            }
        }
        lines.push(line);
    }
    lines
}

/// The rows of a plain-text page in a box of `max`: from row `top` down,
/// padded to one width, with the last row noting any overflow.
fn text_rows(paragraphs: &[String], top: usize, max: Grid) -> Vec<String> {
    let (cols, rows) = (text_cols(max), usize::from(max.rows));
    let all = wrap(paragraphs, cols);
    let top = top.min(all.len().saturating_sub(rows));
    let mut lines = all[top..all.len().min(top + rows)].to_vec();
    let rest = all.len() - top - lines.len();
    if rest > 0 && rows > 1 {
        *lines.last_mut().expect("rows are left") = format!("\u{2193} {} more \u{b7} arrows scroll", rest + 1);
    }
    for line in &mut lines {
        if let Some(cut) = cut_at(line, cols) {
            line.truncate(cut);
        }
        let width = cells(line);
        line.extend(std::iter::repeat_n(' ', cols.saturating_sub(width)));
    }
    lines
}
