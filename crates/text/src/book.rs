//! A laid-out text book: text pages rasterize on demand, pictures decode
//! from the archive or the document.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cosmic_text::{FontSystem, SwashCache, SwashContent};
use parking_lot::{Mutex, MutexGuard};
use yoyomi_core::image::{ImageFormat, Rgba, RgbaImage};
use yoyomi_core::{
    Book, Cancel, Chapter, Encoded, Error, Link, LinkSpan, MAX_PAGE_BYTES, PageKind, Pixels, Result, TextBox, TextLayer, TextLine, check_size,
    decode, decoded_size, image,
};
use zip::ZipArchive;

use crate::layout::{Layout, Line, LinkRange, PAGE_HEIGHT, PAGE_WIDTH, Page, Resource, Source, invalid, read_bounded, zip_entry};

/// How many text layers a book keeps.
const MAX_TEXT_LAYERS: usize = 64;

/// How often a waiting request looks at its `Cancel`.
const CANCEL_POLL: Duration = Duration::from_millis(10);

pub(crate) struct TextBook {
    path: PathBuf,
    title: String,
    pages: Vec<Page>,
    anchors: HashMap<String, usize>,
    /// The outline, or why reading it failed.
    chapters: Result<Vec<Chapter>, String>,
    /// The archive pictures come from; `None` once the book closes.
    zip: Mutex<Option<ZipArchive<BufReader<File>>>>,
    /// The fonts the layout shaped with, which rasterize its glyphs.
    raster: Mutex<(FontSystem, SwashCache)>,
    /// Text layers in the order they were made.
    layers: Mutex<VecDeque<(usize, Arc<TextLayer>)>>,
    closed: Cancel,
}

impl TextBook {
    /// The book `layout` made, titled `title` or else by its file name.
    pub fn new(
        path: &Path,
        title: String,
        layout: Layout,
        zip: Option<ZipArchive<BufReader<File>>>,
        chapters: Result<Vec<Chapter>, String>,
    ) -> TextBook {
        let title = if title.is_empty() { path.file_stem().map_or_else(String::new, |stem| stem.to_string_lossy().into_owned()) } else { title };
        TextBook {
            path: path.to_owned(),
            title,
            pages: layout.pages,
            anchors: layout.anchors,
            chapters,
            zip: Mutex::new(zip),
            raster: Mutex::new((layout.fonts, SwashCache::new())),
            layers: Mutex::new(VecDeque::new()),
            closed: Cancel::new(),
        }
    }

    fn page_at(&self, page: usize) -> Result<&Page> {
        self.pages.get(page).ok_or_else(|| invalid(format!("page {page} out of range")))
    }

    fn lines(&self, page: usize) -> Option<&[Line]> {
        match self.pages.get(page) {
            Some(Page::Text(lines)) => Some(lines),
            _ => None,
        }
    }

    /// The stored bytes of a picture.
    fn bytes(&self, resource: &Resource, cancel: &Cancel) -> Result<Arc<[u8]>> {
        cancel.check()?;
        self.closed.check()?;
        match &resource.source {
            Source::Memory(data) => Ok(data.clone()),
            Source::Zip(index) => {
                let mut zip = lock_or_cancel(&self.zip, cancel)?;
                let zip = zip.as_mut().ok_or(Error::Cancelled)?;
                let data = read_bounded(zip_entry(zip, *index)?, MAX_PAGE_BYTES).map_err(|e| invalid(format!("{}: {e}", resource.name)))?;
                Ok(data.into())
            }
        }
    }

    /// Draws a text page black on white.
    fn rasterize(&self, lines: &[Line], cancel: &Cancel) -> Result<RgbaImage> {
        cancel.check()?;
        self.closed.check()?;
        let mut canvas = RgbaImage::from_pixel(PAGE_WIDTH, PAGE_HEIGHT, Rgba([255, 255, 255, 255]));
        let mut raster = lock_or_cancel(&self.raster, cancel)?;
        let (fonts, swash) = &mut *raster;
        for line in lines.iter().flat_map(|line| std::iter::once(line).chain(&line.annotations)) {
            cancel.check()?;
            for glyph in &line.glyphs {
                let Some(image) = swash.get_image_uncached(fonts, glyph.key) else { continue };
                let (left, top) = (glyph.x + image.placement.left, glyph.y - image.placement.top);
                let (width, height) = (image.placement.width as i32, image.placement.height as i32);
                for row in 0..height {
                    for col in 0..width {
                        let (x, y) = (left + col, top + row);
                        if x < 0 || y < 0 || x >= PAGE_WIDTH as i32 || y >= PAGE_HEIGHT as i32 {
                            continue;
                        }
                        let i = (row * width + col) as usize;
                        let (color, alpha) = match image.content {
                            SwashContent::Mask => ([0, 0, 0], image.data[i]),
                            SwashContent::Color => {
                                let p = &image.data[i * 4..i * 4 + 4];
                                ([p[0], p[1], p[2]], p[3])
                            }
                            SwashContent::SubpixelMask => ([0, 0, 0], image.data[i * 4 + 1]),
                        };
                        let pixel = canvas.get_pixel_mut(x as u32, y as u32);
                        for (channel, ink) in pixel.0.iter_mut().zip(color) {
                            *channel = ((u16::from(*channel) * u16::from(255 - alpha) + u16::from(ink) * u16::from(alpha)) / 255) as u8;
                        }
                    }
                }
            }
        }
        Ok(canvas)
    }

    /// Where an internal link leads: its anchor, else its document's start.
    fn resolve(&self, link: &LinkRange) -> Option<Link> {
        if !link.internal {
            return Some(Link::Url(link.href.clone()));
        }
        let doc = link.href.split_once('#').map_or(link.href.as_str(), |(doc, _)| doc);
        self.anchors.get(&link.href).or_else(|| self.anchors.get(doc)).map(|&page| Link::Page(page))
    }

    /// The selectable text of laid-out lines, in page fractions.
    fn layer(&self, lines: &[Line]) -> TextLayer {
        let lines = lines
            .iter()
            .map(|line| TextLine {
                text: line.text.clone(),
                top: f64::from(line.top) / f64::from(PAGE_HEIGHT),
                bottom: f64::from(line.bottom) / f64::from(PAGE_HEIGHT),
                end: line.end,
                boxes: line
                    .glyphs
                    .iter()
                    .map(|g| TextBox {
                        x0: f64::from(g.left) / f64::from(PAGE_WIDTH),
                        x1: f64::from(g.right) / f64::from(PAGE_WIDTH),
                        index: g.index as usize - line.offset,
                        count: g.runes as usize,
                        rtl: g.rtl,
                    })
                    .collect(),
                links: line
                    .links
                    .iter()
                    .filter_map(|link| Some(LinkSpan { start: link.start - line.offset, end: link.end - line.offset, target: self.resolve(link)? }))
                    .collect(),
            })
            .collect();
        TextLayer { lines }
    }

    #[cfg(test)]
    pub fn anchors(&self) -> &HashMap<String, usize> {
        &self.anchors
    }

    #[cfg(test)]
    pub fn page_lines(&self, page: usize) -> &[Line] {
        self.lines(page).expect("a text page")
    }

    #[cfg(test)]
    pub fn image_name(&self, page: usize) -> Option<&str> {
        match &self.pages[page] {
            Page::Image(resource) => Some(&resource.name),
            Page::Text(_) => None,
        }
    }

    /// The PostScript name of the face that draws rune `rune` of a line.
    #[cfg(test)]
    pub fn face(&self, page: usize, line: usize, rune: usize) -> String {
        let line = &self.page_lines(page)[line];
        let at = (line.offset + rune) as u32;
        let id = line.glyphs.iter().find(|g| (g.index..g.index + g.runes.max(1)).contains(&at)).expect("a glyph draws the rune").key.font_id;
        self.raster.lock().0.db().face(id).map(|face| face.post_script_name.clone()).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn cached_layers(&self) -> Vec<usize> {
        self.layers.lock().iter().map(|(page, _)| *page).collect()
    }
}

/// Lays a picture with pixels that are not fully opaque over white paper,
/// leaving it fully opaque, as text pages are.
fn on_white(picture: &mut RgbaImage) {
    if picture.pixels().all(|p| p.0[3] == 255) {
        return;
    }
    for p in picture.pixels_mut() {
        let alpha = u32::from(p.0[3]);
        for channel in &mut p.0[..3] {
            *channel = ((u32::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
        p.0[3] = 255;
    }
}

/// Locks `mutex`, giving up once `cancel` fires.
fn lock_or_cancel<'a, T>(mutex: &'a Mutex<T>, cancel: &Cancel) -> Result<MutexGuard<'a, T>> {
    loop {
        cancel.check()?;
        if let Some(guard) = mutex.try_lock_for(CANCEL_POLL) {
            return Ok(guard);
        }
    }
}

impl Book for TextBook {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn len(&self) -> usize {
        self.pages.len()
    }

    fn kind(&self, page: usize) -> PageKind {
        if self.lines(page).is_some() { PageKind::Text } else { PageKind::Image }
    }

    /// Text pages invert; pictures keep their colors.
    fn can_invert(&self, page: usize) -> bool {
        self.lines(page).is_some()
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        match self.page_at(page)? {
            Page::Text(lines) => Ok(Arc::new(self.rasterize(lines, cancel)?)),
            Page::Image(resource) => {
                let mut picture = decode(&self.bytes(resource, cancel)?, cancel)?;
                on_white(&mut picture);
                Ok(Arc::new(picture))
            }
        }
    }

    fn page_size(&self, page: usize, cancel: &Cancel) -> Result<(u32, u32)> {
        match self.page_at(page)? {
            Page::Text(_) => Ok((PAGE_WIDTH, PAGE_HEIGHT)),
            Page::Image(resource) => decoded_size(&self.bytes(resource, cancel)?),
        }
    }

    fn chapters(&self, cancel: &Cancel) -> Result<Vec<Chapter>> {
        cancel.check()?;
        self.chapters.clone().map_err(Error::Format)
    }

    fn text_layer(&self, page: usize, cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
        cancel.check()?;
        let Some(lines) = self.lines(page) else { return Ok(None) };
        let mut layers = self.layers.lock();
        if let Some((_, layer)) = layers.iter().find(|(cached, _)| *cached == page) {
            return Ok(Some(layer.clone()));
        }
        let layer = Arc::new(self.layer(lines));
        layers.push_back((page, layer.clone()));
        if layers.len() > MAX_TEXT_LAYERS {
            layers.pop_front();
        }
        Ok(Some(layer))
    }

    fn page_text(&self, page: usize) -> Option<Vec<String>> {
        let lines = self.lines(page)?;
        let mut paragraphs = Vec::new();
        let mut current = String::new();
        for line in lines {
            current.push_str(&line.text);
            if line.end {
                paragraphs.push(current.trim_matches(' ').to_owned());
                current.clear();
            }
        }
        if !current.is_empty() {
            paragraphs.push(current.trim_matches(' ').to_owned());
        }
        Some(paragraphs)
    }

    /// Pictures as stored; text pages as PNG.
    fn encoded(&self, page: usize, cancel: &Cancel) -> Result<Encoded> {
        match self.page_at(page)? {
            Page::Text(lines) => {
                let mut png = Cursor::new(Vec::new());
                self.rasterize(lines, cancel)?.write_to(&mut png, ImageFormat::Png)?;
                Ok(Encoded { bytes: png.into_inner().into(), mime: "image/png" })
            }
            Page::Image(resource) => {
                let bytes = self.bytes(resource, cancel)?;
                let (width, height) = decoded_size(&bytes)?;
                check_size(width, height)?;
                let mime = image::guess_format(&bytes)?.to_mime_type();
                Ok(Encoded { bytes, mime })
            }
        }
    }

    fn close(&self) {
        self.closed.cancel();
        *self.zip.lock() = None;
    }
}
