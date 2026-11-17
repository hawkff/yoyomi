//! What the window shows and what its keys, wheel and clicks do. Nothing here
//! touches AppKit, so it runs and tests on every platform.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use yoyomi_core::{Book, Grid, ImageIds, Jobs, Layer, PageCache, Renderer, Transform};
use yoyomi_view::{PageRequest, Pager, Resident, Shown, Strip, StripPosition, StripRequest, Update, ViewEvent};

use crate::pixels::PixelRenderer;
use crate::{Action, State};

#[cfg(test)]
mod tests;

const ZOOM_STEP: f64 = 1.25;
const ZOOM_MAX: f64 = 8.0;
/// Trackpad points that turn a page in one gesture.
const TURN_POINTS: f64 = 12.0;
/// A pause this long between trackpad events starts a new gesture.
const GESTURE_PAUSE: Duration = Duration::from_millis(150);
/// Points a mouse wheel notch scrolls in webtoon mode.
const NOTCH_POINTS: f64 = 12.0;
/// Milliseconds `+` and `-` add to or take from the glide.
const GLIDE_STEP_MS: i16 = 40;
/// The longest glide, in milliseconds.
const MAX_GLIDE_MS: u16 = 600;
/// Percent of the view's width `[` and `]` take from or add to a theater
/// page.
const WIDTH_STEP: u8 = 5;
/// The narrowest theater page, in percent of the view's width.
const MIN_WIDTH: u8 = 30;

/// One book in the window, shown through a pager and a strip of pixels.
pub(crate) struct Reader {
    book: Arc<PageCache>,
    pager: Pager,
    strip: Strip,
    state: State,
    /// The view in device pixels, once the window has a size.
    size: Option<Grid>,
    /// Whether the strip has shown and holds the webtoon position; until
    /// then `state` holds it.
    strip_shown: bool,
    count: Option<usize>,
    painted: Option<Painted>,
    /// The painted frame arrived after the window last took it.
    fresh: bool,
    error: Option<String>,
    gesture: Gesture,
}

/// The frame on screen and the view it shows, so clicks land on the pages
/// the reader sees even while a newer frame renders.
struct Painted {
    shown: Shown,
    size: Grid,
    webtoon: bool,
    spread: bool,
    quarter_turns: u8,
    zoom: f64,
    center: (f64, f64),
}

/// A trackpad gesture in page mode, which turns at most one page.
#[derive(Default)]
struct Gesture {
    points: f64,
    turned: bool,
    last: Option<Instant>,
}

/// One scroll wheel or trackpad event.
pub(crate) struct Wheel {
    /// AppKit's vertical scrolling delta, in points.
    pub dy: f64,
    /// Whether the delta is in points, as from a trackpad, rather than in
    /// wheel notches.
    pub precise: bool,
    /// Whether the event starts a trackpad gesture.
    pub began: bool,
    /// Whether the event belongs to a trackpad gesture or its momentum.
    pub gesture: bool,
    /// Device pixels per point.
    pub scale: f64,
}

impl Reader {
    /// A reader of `book` from `state`; frames render on `jobs` and arrive
    /// through `events`, which go to `on_view_event`.
    pub fn new(book: Arc<PageCache>, jobs: Jobs, events: Sender<ViewEvent>, state: State) -> Self {
        let renderer: Arc<dyn Renderer> = Arc::new(PixelRenderer);
        // Pixel frames lease no kitty ids.
        let resident = Arc::new(Resident::new(ImageIds::with_base(0), false));
        let pager = Pager::new(book.clone(), renderer.clone(), jobs.clone(), resident.clone(), events.clone());
        let mut strip = Strip::new(book.clone(), renderer, jobs, resident, events);
        // The strip keeps the offset within the page itself.
        let (page, zoom) = (state.page.min(book.len().saturating_sub(1)), state.zoom.clamp(1.0, ZOOM_MAX));
        let (glide_ms, theater_width) = (state.glide_ms.min(MAX_GLIDE_MS), state.theater_width.clamp(MIN_WIDTH, 100));
        let state = State { page, zoom, quarter_turns: state.quarter_turns & 3, glide_ms, theater_width, ..state };
        strip.set_glide(Duration::from_millis(state.glide_ms.into()));
        Reader {
            book,
            pager,
            strip,
            state,
            size: None,
            strip_shown: false,
            count: None,
            painted: None,
            fresh: false,
            error: None,
            gesture: Gesture::default(),
        }
    }

    /// The reading state, with the webtoon position the strip scrolled to.
    pub fn state(&self) -> State {
        let at = self.at();
        State { page: at.page, offset: at.offset, ..self.state.clone() }
    }

    /// Why the painted frame lacks a page, if it does.
    pub fn error(&self) -> Option<String> {
        self.error.clone()
    }

    /// The frame to draw, once after each one arrives.
    pub fn take_frame(&mut self) -> Option<&Shown> {
        let fresh = std::mem::take(&mut self.fresh);
        self.painted.as_ref().filter(|_| fresh).map(|painted| &painted.shown)
    }

    /// Takes the view's new size in device pixels and renders for it.
    pub fn resize(&mut self, width: u32, height: u32) {
        let side = |pixels: u32| pixels.clamp(1, u32::from(u16::MAX)) as u16;
        let size = Grid::new(side(width), side(height));
        if self.size != Some(size) {
            self.size = Some(size);
            self.show();
        }
    }

    /// Takes a finished job of the pager or the strip.
    pub fn on_view_event(&mut self, event: ViewEvent) {
        let (update, webtoon) = match self.pager.on_event(event) {
            Ok(update) => (update, false),
            Err(event) => match self.strip.on_event(event) {
                Ok(update) => (update, true),
                Err(_) => return,
            },
        };
        self.apply(update, webtoon);
    }

    /// Whether webtoon scrolling still runs, so `tick` should run again.
    pub fn moving(&self) -> bool {
        self.state.webtoon && self.strip.moving()
    }

    /// Runs one frame of webtoon scrolling.
    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    /// Runs the frame of webtoon scrolling for the time `now`.
    fn tick_at(&mut self, now: Instant) {
        if self.state.webtoon {
            let update = self.strip.tick(now);
            self.apply(update, true);
        }
    }

    /// A key, by the name the window gives it: its character, or `up`,
    /// `down`, `left` and `right` for the arrows. Returns how the window
    /// closes when the key closes it.
    pub fn key(&mut self, key: &str) -> Option<Action> {
        if let [digit @ b'0'..=b'9'] = key.as_bytes()
            && (*digit != b'0' || self.count.is_some())
        {
            self.count = Some((self.count.unwrap_or(0) * 10 + usize::from(digit - b'0')).min(9999));
            return None;
        }
        let state = &mut self.state;
        let (webtoon, zoomed) = (state.webtoon, state.zoom > 1.001);
        match key {
            "q" => return Some(Action::Clear),
            "Q" => return Some(Action::Save),
            "f" => return Some(Action::Return),
            "s" if !webtoon => {
                state.spread = !state.spread;
                self.reset_view();
            }
            "i" => state.invert = !state.invert,
            // Showing the strip again would drop the scrolling under way.
            "+" | "=" | "-" if webtoon => {
                self.glide_by(if key == "-" { -GLIDE_STEP_MS } else { GLIDE_STEP_MS });
                return None;
            }
            "+" | "=" => state.zoom = (state.zoom * ZOOM_STEP).min(ZOOM_MAX),
            "-" => {
                state.zoom = (state.zoom / ZOOM_STEP).max(1.0);
                if state.zoom <= 1.001 {
                    self.reset_view();
                }
            }
            "0" if !webtoon => self.reset_view(),
            "up" if zoomed && !webtoon => self.pan(0.0, -1.0),
            "down" if zoomed && !webtoon => self.pan(0.0, 1.0),
            "left" if zoomed && !webtoon => self.pan(-1.0, 0.0),
            "right" if zoomed && !webtoon => self.pan(1.0, 0.0),
            // From pages, the strip at the page in view, filling the width
            // with `T`; from the strip at the other width, this width at the
            // same place; from the strip at this width, pages at the page on
            // top.
            "t" | "T" => {
                let theater = key == "T";
                let at = self.at();
                let switch = webtoon && self.state.theater != theater;
                if !switch {
                    self.state.webtoon = !webtoon;
                    self.strip.clear();
                }
                self.state.theater = theater && self.state.webtoon;
                self.state.page = at.page;
                self.state.offset = if switch { at.offset } else { 0.0 };
                self.state.spread = false;
                self.reset_view();
                self.strip_shown = false;
            }
            // The strip at the new width, at the same place.
            "[" | "]" if webtoon && state.theater => {
                let width = if key == "[" { state.theater_width.saturating_sub(WIDTH_STEP) } else { state.theater_width + WIDTH_STEP };
                let at = self.at();
                (self.state.page, self.state.offset) = (at.page, at.offset);
                self.state.theater_width = width.clamp(MIN_WIDTH, 100);
                self.strip_shown = false;
            }
            "R" => state.quarter_turns = (state.quarter_turns + 1) & 3,
            // A count names a one-based page for both.
            "g" | "G" => {
                let (page, offset) = match self.count.take() {
                    Some(count) => (count - 1, 0.0),
                    None if key == "g" => (0, 0.0),
                    None => (self.book.len().saturating_sub(1), if webtoon { 1.0 } else { 0.0 }),
                };
                self.jump(page, offset);
            }
            "j" | "J" | "k" | "K" => {
                let count = self.count.take().unwrap_or(1) as isize;
                let delta = if key == "j" || key == "J" { count } else { -count };
                if webtoon {
                    let fraction = if key == "J" || key == "K" { 0.5 } else { 1.0 };
                    let height = f64::from(self.size.map_or(0, |size| size.rows));
                    self.strip.scroll(height * fraction * delta as f64);
                } else {
                    self.turn(delta);
                }
                return None;
            }
            _ => return None,
        }
        self.show();
        None
    }

    /// A scroll wheel or trackpad event at `now`. Webtoon mode scrolls by
    /// it; page mode turns a page per wheel notch or per trackpad gesture.
    pub fn wheel(&mut self, wheel: Wheel, now: Instant) {
        if self.state.webtoon {
            // A trackpad moves the view with the fingers; a notch glides.
            if wheel.precise {
                self.strip.shift(-wheel.dy * wheel.scale);
            } else {
                self.strip.scroll(-wheel.dy * NOTCH_POINTS * wheel.scale);
            }
            return;
        }
        if !wheel.precise || !wheel.gesture {
            if wheel.dy != 0.0 {
                self.turn(if wheel.dy < 0.0 { 1 } else { -1 });
            }
            return;
        }
        let gesture = &mut self.gesture;
        if wheel.began || gesture.last.is_some_and(|last| now.duration_since(last) >= GESTURE_PAUSE) {
            *gesture = Gesture::default();
        }
        gesture.last = Some(now);
        gesture.points += wheel.dy;
        if !gesture.turned && gesture.points.abs() >= TURN_POINTS {
            gesture.turned = true;
            let delta = if gesture.points < 0.0 { 1 } else { -1 };
            self.turn(delta);
        }
    }

    /// Goes to `page` as a link does, with the whole page in view.
    pub fn go_to(&mut self, page: usize) {
        self.reset_view();
        self.jump(page, 0.0);
    }

    /// The page under the point at fractions `fx`, `fy` of the view in the
    /// painted frame, with the point in fractions of the unturned page.
    pub fn page_point(&self, fx: f64, fy: f64) -> Option<(usize, f64, f64)> {
        if !((0.0..1.0).contains(&fx) && (0.0..1.0).contains(&fy)) {
            return None;
        }
        let painted = self.painted.as_ref()?;
        let (x, y) = (fx * f64::from(painted.size.cols), fy * f64::from(painted.size.rows));
        // shortcut: webtoon clicks map through the strip's current scroll,
        // which runs ahead of the painted frame while one renders; keep the
        // frame's scroll with it if clicks during scrolling go astray.
        if painted.webtoon {
            let (page, px, py) = self.strip.page_at(x, y)?;
            let (px, py) = unturn(painted.quarter_turns, px, py);
            return Some((page, px, py));
        }
        painted.shown.scene.layers.iter().find_map(|layer| {
            let Layer::Pixels { image, at } = layer else { return None };
            let (left, top) = (f64::from(at.col), f64::from(at.row));
            let (width, height) = (f64::from(image.width()), f64::from(image.height()));
            if x < left || x >= left + width || y < top || y >= top + height {
                return None;
            }
            let right = painted.spread && u32::from(at.col) * 2 >= u32::from(painted.size.cols);
            // The zoom crop in fractions of the turned page, as crop_zoom takes it.
            let side = (1.0 / painted.zoom).min(1.0);
            let corner = |center: f64| (center - side / 2.0).max(0.0).min(1.0 - side);
            let (tx, ty) = (corner(painted.center.0) + (x - left) / width * side, corner(painted.center.1) + (y - top) / height * side);
            let (px, py) = unturn(painted.quarter_turns, tx, ty);
            Some((painted.shown.page + usize::from(right), px, py))
        })
    }

    /// Asks the view in use for the current state.
    fn show(&mut self) {
        let Some(grid) = self.size else { return };
        let state = &self.state;
        if state.webtoon {
            let fill = state.theater.then_some(state.theater_width);
            let request = StripRequest { grid, quarter_turns: state.quarter_turns, invert: state.invert, fill };
            let update = self.strip.show(self.at(), request);
            self.strip_shown = true;
            self.apply(update, true);
        } else {
            let transform = Transform {
                quarter_turns: state.quarter_turns,
                zoom: state.zoom,
                center: state.center,
                invert: state.invert,
                ..Transform::default()
            };
            let request = PageRequest {
                page: state.page,
                spread: state.spread,
                grid,
                margin: Grid::default(),
                transform,
                selected_page: state.page,
                text_top: 0,
            };
            let update = self.pager.show(request);
            self.apply(update, false);
        }
    }

    /// Paints the frame of an update from the view in use. The views send
    /// frames of their latest request only, which the current state made.
    fn apply(&mut self, update: Update, webtoon: bool) {
        let Some(shown) = update.shown.filter(|_| webtoon == self.state.webtoon) else { return };
        self.error = (!shown.errors.is_empty()).then(|| shown.errors.join("; "));
        let State { spread, quarter_turns, zoom, center, .. } = self.state;
        self.painted = Some(Painted { shown, size: self.size.unwrap_or_default(), webtoon, spread, quarter_turns, zoom, center });
        self.fresh = true;
    }

    /// The webtoon position: the strip's once it showed, else the state's.
    fn at(&self) -> StripPosition {
        if self.state.webtoon && self.strip_shown {
            self.strip.position()
        } else {
            StripPosition { page: self.state.page, offset: self.state.offset }
        }
    }

    /// Shows `page` from `offset`, its fraction above the top in webtoon mode.
    fn jump(&mut self, page: usize, offset: f64) {
        self.state.page = page.min(self.book.len().saturating_sub(1));
        self.state.offset = offset;
        self.strip_shown = false;
        self.show();
    }

    /// Turns by `delta` pages or spreads: past the end a spread stops on the
    /// last pair rather than a lone last page.
    fn turn(&mut self, delta: isize) {
        let (last, page) = (self.book.len().saturating_sub(1) as isize, self.state.page as isize);
        let step = if self.state.spread { 2 } else { 1 };
        let delta = if self.state.spread && page + delta * step > last { (last - page) / step } else { delta };
        self.state.page = (page + delta * step).clamp(0, last) as usize;
        self.state.offset = 0.0;
        self.show();
    }

    /// `+` and `-` in webtoon mode: a glide `step` milliseconds longer,
    /// within 0 and `MAX_GLIDE_MS`.
    fn glide_by(&mut self, step: i16) {
        self.state.glide_ms = self.state.glide_ms.saturating_add_signed(step).min(MAX_GLIDE_MS);
        self.strip.set_glide(Duration::from_millis(self.state.glide_ms.into()));
    }

    fn reset_view(&mut self) {
        self.state.zoom = 1.0;
        self.state.center = (0.5, 0.5);
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        let state = &mut self.state;
        let (step, half) = (0.15 / state.zoom, 0.5 / state.zoom);
        state.center = ((state.center.0 + dx * step).clamp(half, 1.0 - half), (state.center.1 + dy * step).clamp(half, 1.0 - half));
    }
}

/// A point in fractions of a page turned clockwise by `quarter_turns`, in
/// fractions of the unturned page.
fn unturn(quarter_turns: u8, x: f64, y: f64) -> (f64, f64) {
    match quarter_turns & 3 {
        1 => (y, 1.0 - x),
        2 => (1.0 - x, 1.0 - y),
        3 => (1.0 - y, x),
        _ => (x, y),
    }
}
