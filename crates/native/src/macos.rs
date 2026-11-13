//! The AppKit window: a view that draws the reader's pixel frames and hands
//! it keys, scrolling and clicks. Everything here runs on the main thread;
//! job threads reach it through the main dispatch queue.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent, NSEventPhase,
    NSGraphicsContext, NSResponder, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider, CGImage, CGImageAlphaInfo, CGInterpolationQuality,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
use yoyomi_core::{Book, Cancel, Error, Jobs, Layer, Link, PageCache, Priority, Result, RgbaImage};

use crate::reader::{Reader, Wheel};
use crate::{Action, LinkAt, Outcome, State};

/// Eased webtoon scrolling runs at 120 frames a second.
const FRAME_TIME: Duration = Duration::from_nanos(1_000_000_000 / 120);
/// Points a click may move and still follow a link.
const CLICK_POINTS: f64 = 4.0;

thread_local! {
    /// The view of the open window, for work the main queue runs.
    static VIEW: RefCell<Option<Retained<ReaderView>>> = const { RefCell::new(None) };
}

/// Counts windows, so work for one that closed finds no view.
static SESSIONS: AtomicU64 = AtomicU64::new(0);

struct Ivars {
    session: u64,
    /// The reader until the window closes; closing drops it, which stops its
    /// jobs.
    reader: RefCell<Option<Reader>>,
    outcome: RefCell<Option<Outcome>>,
    book: Arc<PageCache>,
    jobs: Jobs,
    link_at: LinkAt,
    /// The images of the frame on screen and their places in device pixels,
    /// counted from the top left.
    images: RefCell<Vec<(CFRetained<CGImage>, CGRect)>>,
    /// A tick of eased scrolling waits on the main queue.
    ticking: Cell<bool>,
    /// Where the mouse went down, in view points.
    press: Cell<Option<NSPoint>>,
    /// Counts clicks, so a link found for an older one does nothing.
    clicks: Cell<u64>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements, and ReaderView does
    // not implement Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct ReaderView;

    impl ReaderView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.key(event);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.wheel(event);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.cancel_click();
            self.ivars().press.set(Some(self.convertPoint_fromView(event.locationInWindow(), None)));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, _event: &NSEvent) {
            self.cancel_click();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.click(event);
        }
    }

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for ReaderView {}

    // SAFETY: NSWindowDelegate has no safety requirements.
    unsafe impl NSWindowDelegate for ReaderView {
        /// The close button closes as `q` does.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.close(Action::Clear);
            false
        }
    }
);

impl ReaderView {
    fn new(mtm: MainThreadMarker, frame: NSRect, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: initWithFrame: is the designated initializer of NSView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Runs `f` on the reader while the window is open.
    fn with_reader<R>(&self, f: impl FnOnce(&mut Reader) -> R) -> Option<R> {
        self.ivars().reader.borrow_mut().as_mut().map(f)
    }

    /// Device pixels per point.
    fn scale(&self) -> f64 {
        self.window().map(|window| window.backingScaleFactor()).filter(|scale| *scale > 0.0).unwrap_or(1.0)
    }

    /// Renders for the view's size in device pixels and draws the frame on
    /// screen, each image at its place in points.
    fn draw(&self) {
        let (scale, bounds) = (self.scale(), self.bounds());
        let pixels = |points: f64| (points * scale).round().max(1.0) as u32;
        if self.with_reader(|reader| reader.resize(pixels(bounds.size.width), pixels(bounds.size.height))).is_none() {
            return;
        }
        self.take_images();
        let Some(graphics) = NSGraphicsContext::currentContext() else { return };
        let context = graphics.CGContext();
        CGContext::set_rgb_fill_color(Some(&context), 0.0, 0.0, 0.0, 1.0);
        CGContext::fill_rect(Some(&context), bounds);
        CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
        for (image, rect) in self.ivars().images.borrow().iter() {
            // The view counts points from the bottom left.
            let size = CGSize::new(rect.size.width / scale, rect.size.height / scale);
            let origin = CGPoint::new(rect.origin.x / scale, bounds.size.height - rect.origin.y / scale - size.height);
            CGContext::draw_image(Some(&context), CGRect::new(origin, size), Some(image));
        }
    }

    /// Takes the reader's new frame, if one came. Returns whether it did.
    fn take_images(&self) -> bool {
        let Some(Some(images)) = self.with_reader(|reader| reader.take_frame().map(|shown| images(&shown.scene.layers))) else {
            return false;
        };
        *self.ivars().images.borrow_mut() = images;
        true
    }

    /// Draws a new frame and runs the scrolling after any change.
    fn update(&self) {
        if self.take_images() {
            self.setNeedsDisplay(true);
        }
        if self.with_reader(|reader| reader.moving()) == Some(true) && !self.ivars().ticking.replace(true) {
            let session = self.ivars().session;
            let when = DispatchTime::try_from(FRAME_TIME).unwrap_or(DispatchTime::NOW);
            let queued = DispatchQueue::main().after(when, move || with_view(session, ReaderView::tick));
            if queued.is_err() {
                self.ivars().ticking.set(false);
            }
        }
    }

    fn tick(&self) {
        self.ivars().ticking.set(false);
        self.with_reader(Reader::tick);
        self.update();
    }

    fn key(&self, event: &NSEvent) {
        self.cancel_click();
        let Some(c) = event.charactersIgnoringModifiers().and_then(|chars| chars.to_string().chars().next()) else { return };
        let name = match c {
            '\u{f700}' => "up".to_owned(),
            '\u{f701}' => "down".to_owned(),
            '\u{f702}' => "left".to_owned(),
            '\u{f703}' => "right".to_owned(),
            c => c.to_string(),
        };
        match self.with_reader(|reader| reader.key(&name)) {
            Some(Some(action)) => self.close(action),
            Some(None) => self.update(),
            None => {}
        }
    }

    fn wheel(&self, event: &NSEvent) {
        self.cancel_click();
        let wheel = Wheel {
            dy: event.scrollingDeltaY(),
            precise: event.hasPreciseScrollingDeltas(),
            began: event.phase() == NSEventPhase::Began,
            gesture: !event.phase().is_empty() || !event.momentumPhase().is_empty(),
            scale: self.scale(),
        };
        self.with_reader(|reader| reader.wheel(wheel, Instant::now()));
        self.update();
    }

    /// A press and release that moved at most `CLICK_POINTS` follow the link
    /// under them. Text layers may come from a converter, so the lookup runs
    /// on a job.
    fn click(&self, event: &NSEvent) {
        let Some(press) = self.ivars().press.take() else { return };
        let (point, bounds) = (self.convertPoint_fromView(event.locationInWindow(), None), self.bounds());
        if (point.x - press.x).hypot(point.y - press.y) > CLICK_POINTS || bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return;
        }
        let (fx, fy) = (point.x / bounds.size.width, 1.0 - point.y / bounds.size.height);
        let Some(Some((page, x, y))) = self.with_reader(|reader| reader.page_point(fx, fy)) else { return };
        let ivars = self.ivars();
        let (book, link_at, session, click) = (ivars.book.clone(), ivars.link_at, ivars.session, ivars.clicks.get());
        ivars.jobs.spawn(Priority::Visible, &Cancel::new(), move || {
            if let Some(link) = link_at(book.as_ref(), page, x, y) {
                DispatchQueue::main().exec_async(move || with_view(session, |view| view.follow(click, link)));
            }
        });
    }

    fn cancel_click(&self) {
        let ivars = self.ivars();
        ivars.clicks.set(ivars.clicks.get() + 1);
        ivars.press.set(None);
    }

    /// Goes to a page link, or opens a web or mail link in its app.
    fn follow(&self, click: u64, link: Link) {
        if click != self.ivars().clicks.get() {
            return;
        }
        match link {
            Link::Page(page) => {
                self.with_reader(|reader| reader.go_to(page));
                self.update();
            }
            Link::Url(url) => {
                let scheme = url.split(':').next().unwrap_or_default().to_ascii_lowercase();
                if ["http", "https", "mailto"].contains(&scheme.as_str())
                    && let Some(target) = NSURL::URLWithString(&NSString::from_str(&url))
                {
                    NSWorkspace::sharedWorkspace().openURL(&target);
                }
            }
            _ => {}
        }
    }

    /// Keeps the outcome and drops the reader, which stops its jobs. Later
    /// calls change nothing.
    fn end(&self, action: Action) {
        let Some(reader) = self.ivars().reader.take() else { return };
        *self.ivars().outcome.borrow_mut() = Some(Outcome { state: reader.state(), action, error: reader.error() });
        drop(reader);
        self.ivars().images.borrow_mut().clear();
    }

    /// Ends the window with `action`, takes it off the screen and stops the
    /// event loop after the event at hand.
    fn close(&self, action: Action) {
        if self.ivars().reader.borrow().is_none() {
            return;
        }
        self.end(action);
        self.cancel_click();
        if let Some(window) = self.window() {
            window.setDelegate(None);
            window.orderOut(None);
            window.close();
        }
        NSApplication::sharedApplication(self.mtm()).stop(None);
    }
}

/// Runs `f` with the view of window `session`, if it is still open. Only the
/// main thread calls it.
fn with_view(session: u64, f: impl FnOnce(&ReaderView)) {
    let view = VIEW.with(|view| view.borrow().clone());
    if let Some(view) = view.filter(|view| view.ivars().session == session && view.ivars().reader.borrow().is_some()) {
        f(&view);
    }
}

/// The images of a frame's pixel layers with their places.
fn images(layers: &[Layer]) -> Vec<(CFRetained<CGImage>, CGRect)> {
    layers
        .iter()
        .filter_map(|layer| {
            let Layer::Pixels { image, at } = layer else { return None };
            let size = CGSize::new(f64::from(image.width()), f64::from(image.height()));
            Some((cg_image(image)?, CGRect::new(CGPoint::new(f64::from(at.col), f64::from(at.row)), size)))
        })
        .collect()
}

// shortcut: copies the pixels once more per frame; a data provider over the
// shared image would save the copy if scrolling at full Retina size stutters.
fn cg_image(image: &RgbaImage) -> Option<CFRetained<CGImage>> {
    let provider = CGDataProvider::with_cf_data(Some(&CFData::from_bytes(image.as_raw())))?;
    let space = CGColorSpace::new_device_rgb()?;
    let (width, height) = (image.width() as usize, image.height() as usize);
    let alpha = CGBitmapInfo(CGImageAlphaInfo::Last.0);
    // SAFETY: the provider holds `height` rows of `width` RGBA pixels of 8
    // bits each, and the null decode array asks for none.
    unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(&space),
            alpha,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

/// Shows the window until it closes; see `crate::run`.
pub(crate) fn run(book: Arc<PageCache>, jobs: Jobs, state: State, link_at: LinkAt) -> Result<Outcome> {
    let mtm = MainThreadMarker::new().ok_or_else(|| Error::unsupported("the native window needs the main thread"))?;
    let session = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
    let (events, received) = unbounded();
    let reader = Reader::new(book.clone(), jobs.clone(), events, state);
    let previous = NSWorkspace::sharedWorkspace().frontmostApplication();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1100.0, 800.0));
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable;
    // SAFETY: the window is told right below not to release itself when it
    // closes, since no window controller owns it.
    let window =
        unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, style, NSBackingStoreType::Buffered, false) };
    // SAFETY: `window` keeps its own reference until `run` returns.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&format!("yoyomi · {}", book.title())));
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    window.setBackgroundColor(Some(&NSColor::blackColor()));
    window.setMinSize(NSSize::new(320.0, 240.0));

    let ivars = Ivars {
        session,
        reader: RefCell::new(Some(reader)),
        outcome: RefCell::new(None),
        book,
        jobs,
        link_at,
        images: RefCell::new(Vec::new()),
        ticking: Cell::new(false),
        press: Cell::new(None),
        clicks: Cell::new(0),
    };
    let view = ReaderView::new(mtm, frame, ivars);
    window.setContentView(Some(&view));
    window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    window.center();
    window.makeKeyAndOrderFront(None);
    window.makeFirstResponder(Some(&view));
    #[allow(deprecated, reason = "activate() needs macOS 14")]
    app.activateIgnoringOtherApps(true);
    VIEW.with(|slot| *slot.borrow_mut() = Some(view.clone()));

    // Finished jobs wake the main queue, which hands them to the view. The
    // thread ends once the reader and its jobs dropped their senders.
    thread::spawn(move || {
        for event in received {
            DispatchQueue::main().exec_async(move || {
                with_view(session, |view| {
                    view.with_reader(|reader| reader.on_view_event(event));
                    view.update();
                });
            });
        }
    });
    DispatchQueue::main().exec_async(move || {
        with_view(session, |view| {
            if let Some(window) = view.window().filter(|window| window.isVisible()) {
                window.toggleFullScreen(None);
            }
        });
    });
    app.run();

    view.end(Action::Clear);
    VIEW.with(|slot| slot.borrow_mut().take());
    window.setDelegate(None);
    window.setContentView(None);
    if let Some(previous) = previous {
        previous.activateWithOptions(NSApplicationActivationOptions::empty());
    }
    let outcome = view.ivars().outcome.take();
    Ok(outcome.expect("ending the window keeps its outcome"))
}
