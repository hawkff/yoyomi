//! Pagination: XHTML bodies laid out into fixed pages of shaped lines, with
//! images on pages of their own. EPUB, FB2, DOCX and DOC all end up here.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::sync::Arc;

use cosmic_text::{
    Attrs, AttrsList, BufferLine, CacheKey, Ellipsize, Family, FontSystem, Hinting, LayoutGlyph, LayoutLine, LineEnding, Shaping, Style, Weight, Wrap,
};
use unicode_normalization::UnicodeNormalization;
use yoyomi_core::{Chapter, Error, Result};
use zip::ZipArchive;

use crate::fonts::{BUNDLED_FAMILY, font_system};
use crate::path::{link_target, resolve};
use crate::xml::{self, MATHML, MAX_DEPTH, Node, OPS, SVG, XHTML};

pub(crate) const PAGE_WIDTH: u32 = 900;
pub(crate) const PAGE_HEIGHT: u32 = 1200;
pub(crate) const MARGIN: i32 = 60;
const TEXT_WIDTH: f32 = (PAGE_WIDTH as i32 - 2 * MARGIN) as f32;
const BOTTOM: i32 = PAGE_HEIGHT as i32 - MARGIN;
const MAX_PAGES: usize = 10_000;
/// The most bytes one EPUB content document may take.
pub(crate) const MAX_EPUB_DOCUMENT_BYTES: usize = 4 << 20;
/// The most bytes all parsed documents of one book may take together.
pub(crate) const MAX_CONTENT_BYTES: usize = 16 << 20;
pub(crate) const MAX_METADATA_BYTES: usize = 1 << 20;
const MAX_TOKENS: usize = 200_000;
/// The most bytes and XML tokens of one FB2, DOCX or DOC document.
pub(crate) const MAX_DOCUMENT_BYTES: usize = 32 << 20;
pub(crate) const MAX_DOCUMENT_TOKENS: usize = 2_000_000;

/// The attribute through which FB2, DOCX and DOC headings carry their
/// outline depth.
pub(crate) const DEPTH_ATTR: &str = "data-yoyomi-depth";

/// The read buffer under a ZIP archive. On a share without a page cache
/// every read that reaches the file is a network round trip, and the zip
/// crate reads the central directory one field at a time. Through 1 MiB the
/// directory of a large archive arrives in one or two round trips, and a
/// picture in about one per MiB.
const ZIP_BUFFER_BYTES: usize = 1 << 20;

/// Reads the directory of the ZIP archive in `reader` through the buffer.
pub(crate) fn zip_archive<R: Read + Seek>(reader: R) -> zip::result::ZipResult<ZipArchive<BufReader<R>>> {
    ZipArchive::new(BufReader::with_capacity(ZIP_BUFFER_BYTES, reader))
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Format(message.into())
}

/// The file name of `path`, which errors name the book by.
pub(crate) fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Where a file of the book is.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// An entry of the book's ZIP archive, by index.
    Zip(usize),
    /// Bytes decoded from the document, such as FB2 binaries.
    Memory(Arc<[u8]>),
}

/// A file of the book under its own name.
#[derive(Clone, Debug)]
pub(crate) struct Resource {
    pub name: String,
    pub source: Source,
}

/// The files a layout may read, by package name.
#[derive(Default)]
pub(crate) struct Package {
    pub zip: Option<ZipArchive<BufReader<File>>>,
    pub files: HashMap<String, Resource>,
    pub encrypted: HashSet<String>,
    /// Bytes of parsed documents so far.
    pub used: usize,
}

impl Package {
    /// The bytes of `name`, failing past `limit` bytes.
    pub fn read(&mut self, name: &str, limit: usize) -> Result<Vec<u8>> {
        if self.encrypted.contains(name) {
            return Err(invalid(format!("EPUB required resource is encrypted: {name}")));
        }
        let resource = self.files.get(name).ok_or_else(|| invalid(format!("EPUB missing resource: {name}")))?;
        match &resource.source {
            Source::Memory(data) if data.len() > limit => Err(Error::Limit(format!("data exceeds {limit} bytes"))),
            Source::Memory(data) => Ok(data.to_vec()),
            Source::Zip(index) => {
                let zip = self.zip.as_mut().expect("ZIP sources come with their archive");
                read_bounded(zip_entry(zip, *index)?, limit)
            }
        }
    }

    /// Parses `name` as XML, within `limit` bytes and what is left of the
    /// book's budget for parsed documents.
    pub fn document(&mut self, name: &str, limit: usize) -> Result<Node> {
        let data = self.read(name, limit.min(MAX_CONTENT_BYTES.saturating_sub(self.used))).map_err(|e| invalid(format!("EPUB {name}: {e}")))?;
        self.used += data.len();
        xml::parse(&data, MAX_TOKENS).map_err(invalid)
    }
}

pub(crate) fn zip_entry<R: Read + Seek>(zip: &mut ZipArchive<R>, index: usize) -> Result<zip::read::ZipFile<'_, R>> {
    zip.by_index(index).map_err(|e| invalid(e.to_string()))
}

/// Reads to the end, failing past `limit` bytes.
pub(crate) fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(Error::Limit(format!("data exceeds {limit} bytes")));
    }
    Ok(data)
}

/// A shaped glyph, placed on its page.
#[derive(Clone, Debug)]
pub(crate) struct Glyph {
    pub key: CacheKey,
    /// The glyph origin in whole page pixels.
    pub x: i32,
    pub y: i32,
    /// The glyph's box across the page, in pixels.
    pub left: f32,
    pub right: f32,
    /// The glyph's first rune in its paragraph, and how many runes it covers.
    pub index: u32,
    pub runes: u32,
    pub rtl: bool,
}

/// A link over a range of paragraph runes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkRange {
    pub start: usize,
    pub end: usize,
    /// A web address, or the anchor key of an internal reference.
    pub href: String,
    pub internal: bool,
}

/// One line of a text page.
#[derive(Clone, Debug, Default)]
pub(crate) struct Line {
    pub text: String,
    /// The paragraph rune index of the first rune.
    pub offset: usize,
    /// The extent of the glyphs, in page pixels.
    pub top: i32,
    pub bottom: i32,
    /// The line ends its paragraph.
    pub end: bool,
    pub glyphs: Vec<Glyph>,
    pub links: Vec<LinkRange>,
    /// Ruby readings above the line.
    pub annotations: Vec<Line>,
}

#[derive(Debug)]
pub(crate) enum Page {
    Text(Vec<Line>),
    Image(Resource),
}

/// The emphasis of the paragraph text from rune `start` to the next
/// emphasis.
#[derive(Clone, Copy, PartialEq)]
struct Emphasis {
    start: usize,
    italic: bool,
    bold: bool,
}

struct Ruby {
    start: usize,
    end: usize,
    reading: String,
}

/// A paragraph under construction.
#[derive(Default)]
pub(crate) struct Paragraph {
    runes: Vec<char>,
    rubies: Vec<Ruby>,
    links: Vec<LinkRange>,
    emphasis: Vec<Emphasis>,
    /// The chapter whose destination is the paragraph's first line.
    chapter: Option<usize>,
}

impl Paragraph {
    /// Sets the emphasis of the text written next.
    fn style(&mut self, italic: bool, bold: bool) {
        let next = Emphasis { start: self.runes.len(), italic, bold };
        match self.emphasis.last_mut() {
            Some(last) if (last.italic, last.bold) == (italic, bold) => {}
            Some(last) if last.start == next.start => *last = next,
            _ => self.emphasis.push(next),
        }
    }

    /// Appends text with soft hyphens dropped, in NFC, with HTML whitespace
    /// collapsed; no-break spaces stay.
    pub fn write(&mut self, text: &str) {
        for c in text.chars().filter(|&c| c != '\u{ad}').nfc() {
            let c = if " \t\r\n\x0c".contains(c) {
                if self.runes.last().is_none_or(|&last| last == ' ') {
                    continue;
                }
                ' '
            } else {
                c
            };
            self.runes.push(c);
        }
    }

    fn string(&self) -> String {
        self.runes.iter().collect()
    }

    /// Appends a ruby element: its base text inline, each reading for the
    /// base before it.
    fn ruby(&mut self, n: &Node) -> Result<()> {
        fn inline(n: &Node, out: &mut String) -> Result<()> {
            if n.name.local.is_empty() {
                out.push_str(&n.text);
                return Ok(());
            }
            if n.name.space != XHTML {
                return Err(invalid(format!("EPUB unsupported ruby namespace: {:?}", n.name.space)));
            }
            if !matches!(n.name.local.as_str(), "rb" | "rt" | "rp" | "span" | "em" | "strong" | "b" | "i" | "a") {
                return Err(invalid(format!("EPUB unsupported ruby element: {:?}", n.name.local)));
            }
            for child in &n.children {
                if child.name.local == "rt" || child.name.local == "rp" {
                    return Err(invalid("EPUB ruby needs direct rt/rp children"));
                }
                inline(child, out)?;
            }
            Ok(())
        }
        let mut base = String::new();
        for child in &n.children {
            match child.name.local.as_str() {
                "rt" => {
                    let mut reading = String::new();
                    inline(child, &mut reading)?;
                    let mut start = self.runes.len();
                    self.write(&base);
                    base.clear();
                    let mut annotation = Paragraph::default();
                    annotation.write(&reading);
                    let reading = annotation.string().trim_matches(' ').to_owned();
                    let mut end = self.runes.len();
                    while start < end && self.runes[start] == ' ' {
                        start += 1;
                    }
                    while end > start && self.runes[end - 1] == ' ' {
                        end -= 1;
                    }
                    if end == start || reading.is_empty() {
                        return Err(invalid("EPUB ruby needs base text and a reading"));
                    }
                    self.rubies.push(Ruby { start, end, reading });
                }
                "rp" => inline(child, &mut String::new())?,
                _ => inline(child, &mut base)?,
            }
        }
        self.write(&base);
        Ok(())
    }
}

/// The byte offsets of `text` mapped to rune indexes, at char boundaries
/// and at the end.
fn rune_indexes(text: &str) -> Vec<usize> {
    let mut runes = vec![0; text.len() + 1];
    for (n, (i, _)) in text.char_indices().enumerate() {
        runes[i] = n;
    }
    runes[text.len()] = text.chars().count();
    runes
}

/// The first rune a laid-out line shows.
fn first_rune(line: &LayoutLine, runes: &[usize]) -> Option<usize> {
    line.glyphs.iter().map(|g| runes[g.start]).min()
}

/// Places shaped glyphs with the line's origin `x` pixels into the text box
/// and its baseline `baseline` pixels down the page.
fn freeze(glyphs: &[LayoutGlyph], runes: &[usize], x: f32, baseline: i32) -> Vec<Glyph> {
    let x = MARGIN as f32 + x;
    glyphs
        .iter()
        .map(|g| {
            let physical = g.physical((x, baseline as f32), 1.0);
            Glyph {
                key: physical.cache_key,
                x: physical.x,
                y: physical.y,
                left: x + g.x,
                right: x + g.x + g.w,
                index: runes[g.start] as u32,
                runes: (runes[g.end] - runes[g.start]) as u32,
                rtl: g.level.is_rtl(),
            }
        })
        .collect()
}

/// One line of a ruby reading, before its band has a place on the page.
struct Reading {
    text: String,
    laid: LayoutLine,
    /// Rune indexes of the whole reading, which the glyphs index.
    runes: Vec<usize>,
    x: f32,
    ascent: i32,
    descent: i32,
    /// The baseline, down from the top of the line's reading bands.
    baseline: i32,
}

/// The book under construction: its pages, anchors and chapters.
pub(crate) struct Layout {
    pub fonts: FontSystem,
    pub pages: Vec<Page>,
    /// Document name, or name#id, to the page it leads to.
    pub anchors: HashMap<String, usize>,
    pub chapters: Vec<Chapter>,
    lines: Vec<Line>,
    y: i32,
    /// Ids waiting for their content to land on a page, with the rune offset
    /// of the paragraph under construction where they appeared.
    pending: Vec<(String, usize)>,
    /// SVG resources being read, against cycles.
    active_images: HashSet<String>,
}

impl Layout {
    pub fn new() -> Result<Layout> {
        Ok(Layout {
            fonts: font_system()?,
            pages: Vec::new(),
            anchors: HashMap::new(),
            chapters: Vec::new(),
            lines: Vec::new(),
            y: MARGIN,
            pending: Vec::new(),
            active_images: HashSet::new(),
        })
    }

    /// Records the page an internal link key leads to.
    pub fn anchor(&mut self, key: &str, page: usize) {
        self.anchors.insert(key.to_owned(), page);
    }

    /// Resolves pending anchors before paragraph offset `up_to`, or all of
    /// them, to the page that receives the current line or image.
    fn place(&mut self, up_to: usize, all: bool) {
        let page = self.pages.len();
        let (placed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending).into_iter().partition(|(_, at)| all || *at < up_to);
        self.pending = kept;
        for (key, _) in placed {
            self.anchor(&key, page);
        }
    }

    /// Points anchors after the last content of a document at its last page.
    pub fn settle(&mut self) {
        let page = self.pages.len().saturating_sub(1);
        for (key, _) in std::mem::take(&mut self.pending) {
            self.anchor(&key, page);
        }
    }

    pub fn add_page(&mut self, page: Page) -> Result<()> {
        if self.pages.len() >= MAX_PAGES {
            return Err(Error::Limit(format!("EPUB exceeds {MAX_PAGES} pages")));
        }
        self.pages.push(page);
        Ok(())
    }

    pub fn flush_page(&mut self) -> Result<()> {
        self.y = MARGIN;
        if self.lines.is_empty() {
            return Ok(());
        }
        let lines = std::mem::take(&mut self.lines);
        self.add_page(Page::Text(lines))
    }

    /// Adds the picture `name` on a page of its own; an SVG wrapper adds the
    /// pictures it shows. Pictures are read only when their page shows.
    pub fn add_image(&mut self, pkg: &mut Package, name: &str, resources: &HashMap<String, String>) -> Result<()> {
        let media = resources.get(name).ok_or_else(|| invalid(format!("EPUB image is not in manifest: {name}")))?;
        if media == "image/svg+xml" {
            if self.active_images.contains(name) || self.active_images.len() >= MAX_DEPTH {
                return Err(invalid(format!("EPUB SVG resource cycle or traversal limit: {name}")));
            }
            self.active_images.insert(name.to_owned());
            let result = self.svg_resource(pkg, name, resources);
            self.active_images.remove(name);
            return result;
        }
        if !matches!(media.as_str(), "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/bmp") {
            return Err(invalid(format!("EPUB unsupported image resource: {name}")));
        }
        let resource = pkg.files.get(name).ok_or_else(|| invalid(format!("EPUB missing resource: {name}")))?.clone();
        if pkg.encrypted.contains(name) {
            return Err(invalid(format!("EPUB required resource is encrypted: {name}")));
        }
        self.flush_page()?;
        self.place(0, true);
        self.add_page(Page::Image(resource))
    }

    fn svg_resource(&mut self, pkg: &mut Package, name: &str, resources: &HashMap<String, String>) -> Result<()> {
        let doc = pkg.document(name, MAX_EPUB_DOCUMENT_BYTES)?;
        if !doc.is(SVG, "svg") {
            return Err(invalid(format!("EPUB image is not an SVG wrapper: {name}")));
        }
        validate_references(name, &doc)?;
        self.svg(pkg, name, &doc, resources)
    }

    /// Reads an SVG wrapper: groups and `image` references, nothing drawn.
    pub fn svg(&mut self, pkg: &mut Package, name: &str, n: &Node, resources: &HashMap<String, String>) -> Result<()> {
        if !n.name.local.is_empty() && n.name.space != SVG {
            return Err(invalid(format!("EPUB unsupported SVG namespace: {}", n.name.space)));
        }
        if n.attrs.iter().any(|a| a.name.local == "transform" || a.name.local == "style") {
            return Err(invalid("EPUB SVG transforms/styles are unsupported"));
        }
        if !n.attr("id").is_empty() {
            self.pending.push((format!("{name}#{}", n.attr("id")), 0));
        }
        match n.name.local.as_str() {
            "svg" | "g" => {
                for c in &n.children {
                    self.svg(pkg, name, c, resources)?;
                }
            }
            "image" => {
                if n.attr("href").is_empty() {
                    return Err(invalid("EPUB SVG image lacks href"));
                }
                let resolved = resolve(name, n.attr("href")).map_err(invalid)?;
                return self.add_image(pkg, &resolved, resources);
            }
            "title" | "desc" | "metadata" => {}
            "" if !n.text.trim().is_empty() => return Err(invalid("EPUB SVG text is unsupported")),
            "" => {}
            other => return Err(invalid(format!("EPUB SVG drawing is unsupported: {other}"))),
        }
        Ok(())
    }

    /// Lays out the XHTML or SVG document `doc`, read from `name`.
    pub fn document(&mut self, pkg: &mut Package, name: &str, doc: &Node, resources: &HashMap<String, String>) -> Result<()> {
        validate_references(name, doc)?;
        if doc.is(SVG, "svg") {
            return self.svg(pkg, name, doc, resources);
        }
        if !doc.is(XHTML, "html") {
            return Err(invalid(format!("unsupported content root or namespace: {}", doc.name.local)));
        }
        let body = doc.child("body");
        if body.name.local.is_empty() {
            return Err(invalid("XHTML lacks body"));
        }
        let title = doc.child("head").child("title").all_text();
        let (first, first_chapter) = (self.pages.len(), self.chapters.len());
        self.anchor(name, first);
        let mut walker = Walker {
            layout: self,
            pkg,
            resources,
            name,
            text: Paragraph::default(),
            links: Vec::new(),
            heading: false,
            italic: 0,
            bold: 0,
            pre: 0,
            bullet: false,
            pending_chapter: None,
        };
        walker.walk(body)?;
        walker.flush()?;
        if !title.is_empty() && self.chapters.len() == first_chapter {
            self.chapters.push(Chapter { title, page: first, depth: 0 });
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn text(&mut self, text: &str, heading: bool) -> Result<()> {
        let mut p = Paragraph::default();
        p.write(text);
        self.paragraph(p, heading)
    }

    /// Shapes `text` in the bundled family, with fallback fonts for what it
    /// lacks, and wraps it at `width` pixels. Each emphasis styles its runs;
    /// `bold` makes all of the text bold.
    fn shape(&mut self, text: &str, emphasis: &[Emphasis], bold: bool, size: f32, width: f32) -> Vec<LayoutLine> {
        let attrs = |italic: bool, bold: bool| {
            let style = if italic { Style::Italic } else { Style::Normal };
            Attrs::new().family(Family::Name(BUNDLED_FAMILY)).weight(if bold { Weight::BOLD } else { Weight::NORMAL }).style(style)
        };
        let mut list = AttrsList::new(&attrs(false, bold));
        let bytes: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
        let byte = |rune: usize| bytes.get(rune).copied().unwrap_or(text.len());
        for (i, e) in emphasis.iter().enumerate() {
            let end = emphasis.get(i + 1).map_or(text.len(), |next| byte(next.start));
            list.add_span(byte(e.start)..end, &attrs(e.italic, bold || e.bold));
        }
        let mut line = BufferLine::new(text, LineEnding::None, list, Shaping::Advanced);
        line.layout(&mut self.fonts, size, Some(width), Wrap::WordOrGlyph, Ellipsize::None, None, 8, Hinting::Disabled).to_vec()
    }

    /// Lays out one paragraph, line by line, onto as many pages as it needs.
    /// A heading's chapter points at the page of its first line.
    pub fn paragraph(&mut self, mut p: Paragraph, heading: bool) -> Result<()> {
        if p.runes.last() == Some(&' ') {
            p.runes.pop();
        }
        if p.runes.is_empty() {
            return Ok(());
        }
        let (size, leading) = if heading { (40.0, 8) } else { (32.0, 6) };
        let text = p.string();
        let runes = rune_indexes(&text);
        let laid = self.shape(&text, &p.emphasis, heading, size, TEXT_WIDTH);
        let (mut start, mut ruby) = (0, 0);
        for (i, line) in laid.iter().enumerate() {
            let done = i + 1 == laid.len();
            let next = match laid.get(i + 1) {
                Some(next) => first_rune(next, &runes).unwrap_or(start).max(start),
                None => p.runes.len(),
            };
            let (ascent, descent) = (line.max_ascent.ceil() as i32, line.max_descent.ceil() as i32);
            // A reading belongs above the first line of its base, in a band
            // of its own, so readings of adjacent bases cannot overlap.
            let mut readings = Vec::new();
            let mut band = 0;
            while let Some(r) = p.rubies.get(ruby).filter(|r| r.start < next) {
                ruby += 1;
                let (mut left, mut right) = (f32::MAX, f32::MIN);
                for g in line.glyphs.iter().filter(|g| runes[g.start] < r.end && runes[g.end] > r.start) {
                    (left, right) = (left.min(g.x), right.max(g.x + g.w));
                }
                if right <= left {
                    return Err(invalid("EPUB ruby base has no visible width"));
                }
                let width = (right - left).max(64.0);
                let left = (left - (width - (right - left)) / 2.0).min(TEXT_WIDTH - width).max(0.0);
                let reading = r.reading.clone();
                let (lines, height) = self.annotation(&reading, left, width, heading)?;
                readings.extend(lines.into_iter().map(|reading| Reading { baseline: band + reading.baseline, ..reading }));
                band += height;
            }
            let height = band + ascent + descent + leading;
            if height > PAGE_HEIGHT as i32 - 2 * MARGIN {
                return Err(invalid("EPUB text line and ruby exceed page height"));
            }
            if self.y + height > BOTTOM {
                self.flush_page()?;
            }
            if i == 0
                && let Some(chapter) = p.chapter
            {
                self.chapters[chapter].page = self.pages.len();
            }
            self.place(next, done);
            let baseline = self.y + band + ascent;
            let annotations = readings
                .into_iter()
                .map(|r| {
                    let at = self.y + r.baseline;
                    let glyphs = freeze(&r.laid.glyphs, &r.runes, r.x, at);
                    Line { text: r.text, top: at - r.ascent, bottom: at + r.descent, glyphs, ..Line::default() }
                })
                .collect();
            let links = p
                .links
                .iter()
                .filter(|link| link.start < next && link.end > start)
                .map(|link| LinkRange { start: link.start.max(start), end: link.end.min(next), ..link.clone() })
                .collect();
            self.lines.push(Line {
                text: p.runes[start..next].iter().collect(),
                offset: start,
                top: baseline - ascent,
                bottom: baseline + descent,
                end: false,
                glyphs: freeze(&line.glyphs, &runes, 0.0, baseline),
                links,
                annotations,
            });
            self.y += height;
            start = next;
        }
        self.lines.last_mut().expect("a paragraph lays out lines").end = true;
        self.y += 18;
        Ok(())
    }

    /// Lays out a ruby reading at 16 pixels, centered in a band `width`
    /// pixels wide from `left`. Returns the lines, with baselines from the
    /// band's top, and the band's height.
    fn annotation(&mut self, reading: &str, left: f32, width: f32, heading: bool) -> Result<(Vec<Reading>, i32)> {
        let laid = self.shape(reading, &[], heading, 16.0, width);
        let runes = rune_indexes(reading);
        let mut lines = Vec::new();
        let mut height = 0;
        for (i, line) in laid.iter().enumerate() {
            let start = line.glyphs.iter().map(|g| g.start).min().unwrap_or(0);
            let end = laid.get(i + 1).and_then(|next| next.glyphs.iter().map(|g| g.start).min()).unwrap_or(reading.len());
            let (ascent, descent) = (line.max_ascent.ceil() as i32, line.max_descent.ceil() as i32);
            let text = reading[start..end.max(start)].to_owned();
            lines.push(Reading {
                text,
                laid: line.clone(),
                runes: runes.clone(),
                x: left + (width - line.w) / 2.0,
                ascent,
                descent,
                baseline: height + ascent,
            });
            height += ascent + descent + 2;
            if height > PAGE_HEIGHT as i32 - 2 * MARGIN {
                return Err(invalid("EPUB ruby reading exceeds page height"));
            }
        }
        Ok((lines, height + 2))
    }
}

/// Fails on scripts, `base` and references that leave the archive. Link
/// targets, media sources and `srcset` candidates are left to the layout,
/// which never loads them.
fn validate_references(name: &str, n: &Node) -> Result<()> {
    if n.is(OPS, "case") {
        return Ok(()); // epub:switch lays out only its default branch
    }
    if n.name.local == "script" || n.name.local == "base" {
        return Err(invalid(format!("EPUB unsupported element: {}", n.name.local)));
    }
    for a in &n.attrs {
        let attr = a.name.local.as_str();
        if attr == "href" && n.is(XHTML, "a") {
            continue;
        }
        if matches!(n.name.local.as_str(), "audio" | "video" | "source" | "track" | "iframe" | "embed") && (attr == "src" || attr == "poster") {
            continue; // media shows its fallback text and never loads
        }
        if matches!(attr, "href" | "src" | "poster") {
            resolve(name, &a.value).map_err(invalid)?;
        }
    }
    n.children.iter().try_for_each(|c| validate_references(name, c))
}

fn is_block(local: &str) -> bool {
    matches!(
        local,
        "p" | "div"
            | "section"
            | "article"
            | "blockquote"
            | "li"
            | "ul"
            | "ol"
            | "dl"
            | "dt"
            | "dd"
            | "pre"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "tr"
            | "td"
            | "th"
            | "figure"
            | "figcaption"
            | "hr"
    )
}

/// The level of an `h1` to `h6` element.
fn heading_level(local: &str) -> Option<u8> {
    match local.as_bytes() {
        [b'h', level @ b'1'..=b'6'] => Some(level - b'0'),
        _ => None,
    }
}

/// Keeps every space of a pre line, a tab as four, through no-break spaces,
/// which the paragraph writer and the fonts preserve; interior blank lines
/// stay blank. Past the indentation the last space of each run stays a
/// plain space, so long lines can still wrap there.
fn pre_line(line: &str, interior: bool) -> String {
    if line.trim().is_empty() {
        return if interior { "\u{a0}" } else { "" }.to_owned();
    }
    let lead = line.len() - line.trim_start_matches([' ', '\t']).len();
    let mut out = line[..lead].replace('\t', "\u{a0}\u{a0}\u{a0}\u{a0}").replace(' ', "\u{a0}");
    let mut chars = line[lead..].chars().peekable();
    while let Some(c) = chars.next() {
        if c != ' ' && c != '\t' {
            out.push(c);
            continue;
        }
        if c == '\t' {
            out.push_str("\u{a0}\u{a0}\u{a0}");
        }
        out.push(if matches!(chars.peek(), Some(' ' | '\t')) { '\u{a0}' } else { ' ' });
    }
    out
}

/// The walk over one document body, with the inline state it carries.
struct Walker<'a> {
    layout: &'a mut Layout,
    pkg: &'a mut Package,
    resources: &'a HashMap<String, String>,
    name: &'a str,
    text: Paragraph,
    /// Open links; `start` indexes the current paragraph.
    links: Vec<LinkRange>,
    heading: bool,
    italic: u32,
    bold: u32,
    pre: u32,
    /// The next text of a list item gets its bullet.
    bullet: bool,
    pending_chapter: Option<usize>,
}

impl Walker<'_> {
    /// Adds the item bullet before its first text; the bullet is not part
    /// of a link that starts there.
    fn marker(&mut self) {
        if !self.bullet {
            return;
        }
        let before = self.text.runes.len();
        self.text.write("\u{2022} ");
        let after = self.text.runes.len();
        for link in self.links.iter_mut().filter(|link| link.start == before) {
            link.start = after;
        }
        self.bullet = false;
    }

    /// Lays out the paragraph under construction, with the open links over
    /// its text.
    fn flush(&mut self) -> Result<()> {
        let mut paragraph = std::mem::take(&mut self.text);
        let len = paragraph.runes.len();
        for link in &mut self.links {
            if link.start < len {
                paragraph.links.push(LinkRange { end: len, ..link.clone() });
            }
            link.start = 0;
        }
        if self.heading && self.pending_chapter.is_some() && !paragraph.string().trim().is_empty() {
            paragraph.chapter = self.pending_chapter.take();
        }
        self.layout.paragraph(paragraph, self.heading)
    }

    fn walk(&mut self, n: &Node) -> Result<()> {
        if n.name.local.is_empty() {
            let lines: Vec<&str> = if self.pre > 0 { n.text.split('\n').collect() } else { vec![&n.text] };
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    self.flush()?;
                }
                let line = if self.pre > 0 { pre_line(line, i > 0 && i + 1 < lines.len()) } else { (*line).to_owned() };
                if !line.trim().is_empty() {
                    self.marker();
                    self.text.style(self.italic > 0, self.bold > 0);
                }
                self.text.write(&line);
            }
            return Ok(());
        }
        match n.name.local.as_str() {
            "script" | "base" => return Err(invalid(format!("unsupported EPUB element: {}", n.name.local))),
            "style" | "embed" => return Ok(()),
            // epub:switch falls back to its plain XHTML default branch.
            "switch" if n.name.space == OPS => {
                return match n.children.iter().find(|c| c.is(OPS, "default")) {
                    Some(default) => self.walk(default),
                    None => Ok(()),
                };
            }
            // MathML reads as its alternative text when it has one, else as its text.
            "math" if n.name.space == MATHML && !n.attr("alttext").trim().is_empty() => return self.walk(&Node::text(n.attr("alttext"))),
            _ => {}
        }
        let local = n.name.local.as_str();
        let italic = u32::from(matches!(local, "i" | "em" | "cite" | "dfn" | "var"));
        let bold = u32::from(matches!(local, "b" | "strong"));
        let pre = u32::from(local == "pre");
        (self.italic, self.bold, self.pre) = (self.italic + italic, self.bold + bold, self.pre + pre);
        let result = self.element(n);
        (self.italic, self.bold, self.pre) = (self.italic - italic, self.bold - bold, self.pre - pre);
        result
    }

    fn element(&mut self, n: &Node) -> Result<()> {
        if n.name.space != XHTML && n.name.local != "svg" {
            // Foreign markup such as MathML or Word's o:p contributes only its text.
            return n.children.iter().try_for_each(|c| self.walk(c));
        }
        let local = n.name.local.as_str();
        let level = heading_level(local);
        let block = is_block(local);
        if block || matches!(local, "br" | "img" | "svg") {
            self.flush()?;
        }
        if let Some(level) = level {
            // Chapter headings start a page, as page-break-before does in most EPUB styles.
            if level <= 2 {
                self.layout.flush_page()?;
            }
            let title = n.all_text();
            if !title.is_empty() {
                let depth = n.attr(DEPTH_ATTR).parse::<i64>().map_or(usize::from(level - 1), |depth| depth.clamp(0, MAX_DEPTH as i64) as usize);
                self.pending_chapter = Some(self.layout.chapters.len());
                self.layout.chapters.push(Chapter { title, page: self.layout.pages.len(), depth });
            }
        }
        let item = local == "li";
        if item {
            // The bullet joins the item's first text, even inside a nested block.
            self.bullet = true;
        }
        if !n.attr("id").is_empty() {
            self.layout.pending.push((format!("{}#{}", self.name, n.attr("id")), self.text.runes.len()));
        }
        let mut link = false;
        if local == "a" {
            if !n.attr("name").is_empty() {
                self.layout.pending.push((format!("{}#{}", self.name, n.attr("name")), self.text.runes.len()));
            }
            if let Some((href, internal)) = link_target(self.name, n.attr("href")) {
                self.links.push(LinkRange { start: self.text.runes.len(), end: 0, href, internal });
                link = true;
            }
        }
        let result = self.contents(n, level.is_some(), block);
        if link {
            let open = self.links.pop().expect("the link pushed above");
            if open.start < self.text.runes.len() {
                self.text.links.push(LinkRange { end: self.text.runes.len(), ..open });
            }
        }
        if item {
            self.bullet = false;
        }
        result
    }

    fn contents(&mut self, n: &Node, heading: bool, block: bool) -> Result<()> {
        let outer = self.heading;
        self.heading |= heading;
        match n.name.local.as_str() {
            "img" => {
                if n.attr("src").is_empty() {
                    return Err(invalid("EPUB image lacks src"));
                }
                let resolved = resolve(self.name, n.attr("src")).map_err(invalid)?;
                return self.layout.add_image(self.pkg, &resolved, self.resources);
            }
            "svg" => return self.layout.svg(self.pkg, self.name, n, self.resources),
            "ruby" => {
                self.marker();
                return self.text.ruby(n);
            }
            "rt" | "rp" | "rtc" => return Err(invalid(format!("EPUB ruby annotation outside ruby: {}", n.name.local))),
            _ => {
                for c in &n.children {
                    self.walk(c)?;
                }
            }
        }
        if block || n.name.local == "br" {
            self.flush()?;
        }
        if heading {
            self.pending_chapter = None;
        }
        self.heading = outer;
        Ok(())
    }
}

/// Lays out one XHTML body as a whole book, as FB2, DOCX and DOC books are.
/// `file` names the book in errors.
pub(crate) fn layout_body(file: &str, pkg: &mut Package, body: Node, resources: &HashMap<String, String>) -> Result<Layout> {
    let mut layout = Layout::new()?;
    let html = Node { children: vec![body], ..Node::element(XHTML, "html") };
    layout.document(pkg, "", &html, resources).map_err(|e| invalid(format!("{file}: {e}")))?;
    layout.flush_page()?;
    layout.settle();
    if layout.pages.is_empty() {
        return Err(invalid(format!("{file}: no readable content")));
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use yoyomi_core::{Book, Cancel};

    use super::*;
    use crate::book::TextBook;

    fn text_lines(layout: &Layout) -> Vec<&Line> {
        layout.pages.iter().flat_map(|page| if let Page::Text(lines) = page { lines.iter().collect() } else { Vec::new() }).collect()
    }

    #[test]
    fn long_text_paginates_without_losing_text() {
        let mut l = Layout::new().unwrap();
        let text = format!("{}{}", "word ".repeat(1200), "X".repeat(100));
        l.text(&text, false).unwrap();
        l.flush_page().unwrap();
        assert!(l.pages.len() >= 2, "long text did not paginate");
        let mut all = String::new();
        for line in text_lines(&l) {
            assert!(line.bottom <= BOTTOM && !line.text.is_empty(), "{line:?}");
            let mut ink = line.glyphs.iter().filter(|g| line.text.chars().nth(g.index as usize - line.offset) != Some(' '));
            assert!(ink.all(|g| g.right <= (PAGE_WIDTH as i32 - MARGIN) as f32 + 0.5), "long word failed to wrap");
            all.push_str(&line.text.replace(' ', ""));
        }
        assert_eq!(all, text.replace(' ', ""), "pagination lost text");
        l.text("\u{10ffff}", false).expect("a missing glyph renders as notdef");
    }

    /// Needs installed fonts for each script; a script none covers is skipped.
    #[test]
    fn installed_fonts_cover_other_scripts() {
        for text in ["手ひらがな「漢字」、。", "q\u{301}", "مرحبا بالعالم", "abc אבג 123"] {
            let mut l = Layout::new().unwrap();
            if l.shape(text, &[], false, 32.0, TEXT_WIDTH).iter().flat_map(|line| &line.glyphs).any(|g| g.glyph_id == 0) {
                eprintln!("skipped {text:?}: no installed font covers it");
                continue;
            }
            let source = text.repeat(70);
            l.text(&source, false).unwrap();
            l.flush_page().unwrap();
            let mut all = String::new();
            for line in text_lines(&l) {
                assert!(!line.text.starts_with(['」', '、', '。']), "CJK closing punctuation starts a line: {:?}", line.text);
                all.push_str(&line.text);
            }
            assert_eq!(all, source, "lost text");
            let book = TextBook::new(Path::new("/books/b.epub"), String::new(), l, None, Ok(Vec::new()));
            assert_eq!(book.page(0, &Cancel::new()).unwrap().dimensions(), (PAGE_WIDTH, PAGE_HEIGHT));
        }
    }

    #[test]
    fn the_page_cap_holds() {
        let mut l = Layout::new().unwrap();
        l.pages = (0..MAX_PAGES).map(|_| Page::Text(Vec::new())).collect();
        assert!(l.add_page(Page::Text(Vec::new())).is_err());
    }

    #[test]
    fn wrapping_keeps_no_break_spaces_and_graphemes() {
        let mut l = Layout::new().unwrap();
        for text in ["one\u{a0}two three four", "a\u{200d}b c\u{200d}d e\u{200d}f", "ABC \u{202e}DEF\u{202c} GHI"] {
            let runes = rune_indexes(text);
            let starts: Vec<usize> = l.shape(text, &[], false, 32.0, 150.0).iter().filter_map(|line| first_rune(line, &runes)).collect();
            assert!(starts.windows(2).all(|w| w[0] < w[1]), "{text:?}: {starts:?}");
            if text.starts_with("one") {
                assert!(starts.len() > 1 && starts[1] >= "one\u{a0}two".chars().count(), "broke at NBSP: {starts:?}");
            }
            let chars: Vec<char> = text.chars().collect();
            assert!(starts.iter().all(|&s| s == 0 || chars[s - 1] != '\u{200d}' && chars[s] != '\u{200d}'), "split a joined cluster: {starts:?}");
        }
        let lines = l.shape("ABC \u{202e}DEF\u{202c} GHI", &[], false, 32.0, TEXT_WIDTH);
        assert!(lines[0].glyphs.iter().any(|g| g.level.is_rtl()), "the bidi override gave no RTL run");
    }

    #[test]
    fn ruby_moves_with_its_base_and_bounds_its_reading() {
        let mut l = Layout::new().unwrap();
        l.y = BOTTOM - 80;
        l.text("previous", false).unwrap();
        let mut p = Paragraph::default();
        p.write("word");
        p.rubies = vec![Ruby { start: 0, end: 4, reading: "reading".into() }];
        l.paragraph(p, false).unwrap();
        l.flush_page().unwrap();
        assert_eq!(l.pages.len(), 2, "ruby did not move with its base");
        let Page::Text(lines) = &l.pages[1] else { panic!("text page") };
        let reading = &lines[0].annotations[0];
        assert_eq!(reading.text, "reading");
        assert!(reading.top >= MARGIN && reading.bottom < lines[0].top, "ruby outside its band: {reading:?} {:?}", lines[0].top);
        assert_eq!(f32::from_bits(reading.glyphs[0].key.font_size_bits), 16.0, "the reading is not smaller");
        let mut p = Paragraph::default();
        p.write("word");
        p.rubies = vec![Ruby { start: 0, end: 4, reading: "reading ".repeat(1000) }];
        assert!(l.paragraph(p, false).unwrap_err().to_string().contains("height"));
    }

    fn ruby(markup: &str) -> Result<Paragraph> {
        let doc = xml::parse(format!(r#"<ruby xmlns="{XHTML}">{markup}</ruby>"#).as_bytes(), 100).unwrap();
        let mut p = Paragraph::default();
        p.ruby(&doc).map(|()| p)
    }

    #[test]
    fn ruby_markup_outside_the_simple_form_fails() {
        let p = ruby("Base<rp>(</rp><rt>reading</rt><rp>)</rp>").unwrap();
        assert_eq!((p.string().as_str(), p.rubies[0].reading.as_str()), ("Base", "reading"));
        for markup in [
            "base<rtc><rt>reading</rt></rtc>",
            "base<ruby>nested<rt>reading</rt></ruby>",
            r#"base<rt xmlns="urn:wrong">reading</rt>"#,
            "<rt>reading</rt>",
        ] {
            assert!(ruby(markup).is_err(), "accepted {markup}");
        }
    }

    #[test]
    fn writes_collapse_html_whitespace_only() {
        let mut p = Paragraph::default();
        p.write("  co\u{ad}operate \t Cafe\u{301}\u{a0} ");
        assert_eq!(p.string(), "cooperate Café\u{a0} ");
        assert_eq!(pre_line("\t a", true), "\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}a");
        assert_eq!(pre_line("a b  c\td", true), "a b\u{a0} c\u{a0}\u{a0}\u{a0} d");
        assert_eq!((pre_line("  ", true), pre_line("", false)), ("\u{a0}".to_owned(), String::new()));
    }
}
