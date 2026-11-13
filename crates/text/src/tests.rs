//! Books opened end to end: EPUB, FB2 and DOCX books, their chapters and text
//! selection. The fixtures are built here.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tempfile::TempDir;
use yoyomi_core::image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use yoyomi_core::{Book, Cancel, Chapter, Link, LinkSpan, PageKind};
use yoyomi_formats::Opened;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::book::TextBook;
use crate::layout::{
    LinkRange, MARGIN, MAX_CONTENT_BYTES, MAX_METADATA_BYTES, PAGE_HEIGHT, PAGE_WIDTH, Package, Resource, Source, read_bounded, zip_archive,
    zip_entry,
};
use crate::{docx, epub, fb2};

type Entry = (String, Vec<u8>);
type Entries = Vec<Entry>;

fn entry(name: &str, data: impl AsRef<[u8]>) -> Entry {
    (name.to_owned(), data.as_ref().to_vec())
}

fn chapter(title: &str, page: usize, depth: usize) -> Chapter {
    Chapter { title: title.to_owned(), page, depth }
}

fn cancel() -> Cancel {
    Cancel::new()
}

fn epub_fixture() -> Entries {
    vec![
        entry("mimetype", "application/epub+zip"),
        entry(
            "META-INF/container.xml",
            r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="OPS/book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        entry(
            "OPS/book.opf",
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>A Text Book</dc:title></metadata><manifest><item id="last" href="text/a.xhtml" media-type="application/xhtml+xml"/><item id="first" href="text/z.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="first"/><itemref idref="last" linear="no"/></spine></package>"#,
        ),
        entry(
            "OPS/text/a.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Notes</title></head><body><p>Last note.</p></body></html>"#,
        ),
        entry(
            "OPS/text/z.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>First document</title></head><body><h1>First heading</h1><p>Hello <em>reader</em> &amp; friends.</p><p>Second paragraph.</p></body></html>"#,
        ),
    ]
}

fn replace(entries: &mut [Entry], name: &str, old: &str, new: &str) {
    for (entry, data) in entries.iter_mut().filter(|(entry, _)| entry == name) {
        let text = String::from_utf8(std::mem::take(data)).unwrap();
        assert!(text.contains(old), "{entry} lacks {old:?}");
        *data = text.replace(old, new).into_bytes();
    }
}

fn write_zip(entries: &[Entry], ext: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("book{ext}"));
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    for (name, data) in entries {
        zip.start_file(name.as_str(), SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
    (dir, path)
}

fn open_epub(entries: &[Entry]) -> yoyomi_core::Result<TextBook> {
    let (_dir, path) = write_zip(entries, ".epub");
    epub::open(&path)
}

/// Opens `path` the way the app does: `formats::open`, then this crate.
fn open_any(path: &Path) -> Arc<dyn Book> {
    match yoyomi_formats::open(path).unwrap() {
        Opened::Text(format) => crate::open(path, &format).unwrap(),
        other => panic!("{other:?}"),
    }
}

/// A 3 x 5 PNG with one red pixel.
fn png() -> Vec<u8> {
    let mut image = RgbaImage::new(3, 5);
    image.put_pixel(1, 1, Rgba([255, 0, 0, 255]));
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

/// The lines of a text page, one per row, without trailing spaces.
fn text(book: &TextBook, page: usize) -> String {
    book.page_lines(page).iter().map(|line| line.text.trim_end_matches(' ')).collect::<Vec<_>>().join("\n")
}

#[test]
fn epub_spine_orders_pages_and_text() {
    for version in ["2.0", "3.0"] {
        let mut entries = epub_fixture();
        replace(&mut entries, "OPS/book.opf", r#"version="3.0""#, &format!(r#"version="{version}""#));
        let (_dir, path) = write_zip(&entries, ".epub");
        let book = epub::open(&path).unwrap();
        assert_eq!((book.title(), book.len(), book.path()), ("A Text Book", 2, path.as_path()));
        assert_eq!(text(&book, 0), "First heading\nHello reader & friends.\nSecond paragraph.");
        assert_eq!(text(&book, 1), "Last note.");
        assert_eq!(book.chapters(&cancel()).unwrap(), [chapter("First heading", 0, 0), chapter("Notes", 1, 0)]);
        let page = book.page(0, &cancel()).unwrap();
        assert_eq!(page.dimensions(), (PAGE_WIDTH, PAGE_HEIGHT));
        let margin = MARGIN as u32;
        let ink = (margin..150).any(|y| (margin..PAGE_WIDTH - margin).any(|x| page.get_pixel(x, y).0[0] < 255));
        assert!(ink, "text raster is blank");
        let encoded = book.encoded(0, &cancel()).unwrap();
        assert_eq!(encoded.mime, "image/png");
        let reopened = epub::open(&path).unwrap();
        assert_eq!(reopened.len(), book.len());
        assert_eq!(reopened.encoded(0, &cancel()).unwrap().bytes, encoded.bytes, "pagination or raster changed on reopen");
        assert_eq!(book.kind(book.len()), PageKind::Image);
        assert!(!book.can_invert(book.len()));
        let book = Arc::new(book);
        let renders: Vec<_> = (0..3)
            .map(|_| {
                let book = book.clone();
                thread::spawn(move || book.encoded(0, &Cancel::new()).unwrap().bytes)
            })
            .collect();
        for render in renders {
            assert_eq!(render.join().unwrap(), encoded.bytes);
        }
    }
}

#[test]
fn epub_pictures_take_pages_in_document_order() {
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/book.opf", "</manifest>", r#"<item id="picture" href="images/panel%201" media-type="image/png"/></manifest>"#);
    replace(
        &mut entries,
        "OPS/text/z.xhtml",
        "<body>",
        r#"<body><p>Before<img src="../images/panel%201#view"/>After</p><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="../images/panel%201"/></svg>"#,
    );
    entries.extend([entry("OPS/images/panel 1", png()), entry("unused.png", png())]);
    let (_dir, path) = write_zip(&entries, ".zip");
    let book = epub::open(&path).unwrap();
    assert_eq!(book.len(), 6);
    for page in 0..=book.len() {
        assert_eq!(book.can_invert(page), book.kind(page) == PageKind::Text, "page {page}");
    }
    assert_eq!((text(&book, 0).as_str(), text(&book, 2).as_str(), text(&book, 5).as_str()), ("Before", "After", "Last note."));
    assert!(text(&book, 4).starts_with("First heading"));
    for page in [1, 3] {
        assert_eq!(book.image_name(page), Some("OPS/images/panel 1"));
        assert_eq!(book.page(page, &cancel()).unwrap().dimensions(), (3, 5));
        assert_eq!(book.page_size(page, &cancel()).unwrap(), (3, 5));
        assert_eq!(book.encoded(page, &cancel()).unwrap().mime, "image/png");
        assert!(book.page_text(page).is_none() && book.text_layer(page, &cancel()).unwrap().is_none());
    }
}

#[test]
fn an_image_only_epub_keeps_its_colors() {
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/book.opf", r#"href="text/z.xhtml" media-type="application/xhtml+xml""#, r#"href="picture" media-type="image/png""#);
    replace(&mut entries, "OPS/book.opf", r#"<itemref idref="last" linear="no"/>"#, "");
    entries.push(entry("OPS/picture", png()));
    let book = open_epub(&entries).unwrap();
    assert_eq!((book.len(), book.kind(0), book.can_invert(0)), (1, PageKind::Image, false));
    assert_eq!(book.page(0, &cancel()).unwrap().dimensions(), (3, 5));
    assert_eq!(book.encoded(0, &cancel()).unwrap().mime, "image/png");
}

#[test]
fn invalid_epubs_fail_with_their_reason() {
    let cases = [
        ("OPS/book.opf", r#"<itemref idref="first"/><itemref idref="last" linear="no"/>"#, "", "spine"),
        ("OPS/book.opf", r#"idref="first""#, r#"idref="absent""#, "missing manifest"),
        ("OPS/book.opf", r#"id="last""#, r#"id="first""#, "duplicate manifest"),
        ("OPS/book.opf", "text/z.xhtml", "https://example.org/ch.xhtml", "local resource"),
        ("OPS/book.opf", "text/z.xhtml", "text/missing.xhtml", "missing resource"),
        ("OPS/book.opf", "application/xhtml+xml", "text/html", "unsupported spine"),
        ("OPS/text/z.xhtml", "<h1>First heading</h1><p>Hello <em>reader</em> &amp; friends.</p><p>Second paragraph.</p>", "", "no readable content"),
        ("OPS/text/z.xhtml", "</html>", "", "XML"),
        ("OPS/text/z.xhtml", "</html>", "</html><extra/>", "one complete root"),
        ("OPS/text/z.xhtml", "<body>", "<body><script>alert(1)</script>", "unsupported"),
        ("OPS/text/z.xhtml", "<body>", r#"<body><img src="https://example.org/p.png"/>"#, "local resource"),
        ("OPS/text/z.xhtml", "<head>", r#"<head><link href="https://example.org/style.css"/>"#, "local resource"),
        ("OPS/text/z.xhtml", "<body>", r#"<body><img src="../../../p.png"/>"#, "escapes"),
        ("OPS/text/z.xhtml", "<body>", r#"<body><img src="panel.png"/>"#, "not in manifest"),
        ("OPS/text/z.xhtml", "<body>", r#"<body><svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0"/></svg>"#, "SVG drawing"),
        ("OPS/text/z.xhtml", "<body>", r#"<body xml:base="../">"#, "xml:base"),
        ("OPS/book.opf", r#"<itemref idref="first"/>"#, r#"<itemref idref="first" linear="no"/>"#, "linear content"),
        ("OPS/book.opf", "http://www.idpf.org/2007/opf", "urn:wrong", "namespace"),
    ];
    for (file, old, new, want) in cases {
        let mut entries = epub_fixture();
        replace(&mut entries, file, old, new);
        let error = open_epub(&entries).err().unwrap_or_else(|| panic!("accepted {new:?}")).to_string();
        assert!(error.contains(want), "{error}, want {want:?}");
    }
    for name in ["../escape", "/absolute", "OPS/../alias", "OPS\\bad"] {
        let mut entries = epub_fixture();
        entries.push(entry(name, "x"));
        assert!(open_epub(&entries).is_err(), "accepted the ZIP entry {name:?}");
    }
    assert!(open_epub(&[entry("cover.png", png())]).is_err(), "accepted a missing container");
}

#[test]
fn encrypted_resources_fail_only_when_read() {
    for required in [false, true] {
        let reference = if required { "OPS/text/z.xhtml" } else { "OPS/fonts/font.otf" };
        let mut entries = epub_fixture();
        entries.push(entry("OPS/fonts/font.otf", "obfuscated font"));
        entries.push(entry(
            "META-INF/encryption.xml",
            format!(
                r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#"><EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/><CipherData><CipherReference URI="{reference}"/></CipherData></EncryptedData></encryption>"#
            ),
        ));
        replace(&mut entries, "OPS/book.opf", "</manifest>", r#"<item id="font" href="fonts/font.otf" media-type="font/otf"/></manifest>"#);
        match open_epub(&entries) {
            Ok(_) => assert!(!required, "accepted encrypted required text"),
            Err(error) => assert!(required && error.to_string().contains("encrypted"), "{error}"),
        }
    }
}

#[test]
fn epub_limits_hold() {
    let mut entries = epub_fixture();
    entries[1].1 = vec![b' '; MAX_METADATA_BYTES + 1];
    assert!(open_epub(&entries).is_err(), "accepted oversized metadata");
    let mut pkg = Package { used: MAX_CONTENT_BYTES - 3, ..Package::default() };
    pkg.files.insert("x".into(), Resource { name: "x".into(), source: Source::Memory(Arc::from(&b"<r/>"[..])) });
    assert!(pkg.document("x", MAX_METADATA_BYTES).is_err(), "accepted aggregate content overflow");
    let many: Entries = (0..10_001).map(|i| entry(&format!("f{i}"), "")).collect();
    assert!(open_epub(&many).err().unwrap().to_string().contains("entries"));
}

#[test]
fn svg_wrappers_show_their_picture_and_cycles_fail() {
    for cycle in [false, true] {
        let mut entries = epub_fixture();
        replace(
            &mut entries,
            "OPS/book.opf",
            "</manifest>",
            r#"<item id="wrapper" href="images/wrapper.svg" media-type="image/svg+xml"/><item id="panel" href="images/panel.png" media-type="image/png"/></manifest>"#,
        );
        replace(&mut entries, "OPS/text/z.xhtml", "<body>", r#"<body><img src="../images/wrapper.svg"/>"#);
        let reference = if cycle { "wrapper.svg" } else { "panel.png" };
        entries.push(entry("OPS/images/wrapper.svg", format!(r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="{reference}"/></svg>"#)));
        entries.push(entry("OPS/images/panel.png", png()));
        match open_epub(&entries) {
            Ok(book) => assert!(!cycle && book.image_name(0) == Some("OPS/images/panel.png"), "wrapper lost its picture"),
            Err(error) => assert!(cycle && error.to_string().contains("cycle"), "{error}"),
        }
    }
}

/// A BMP header of 50000 x 50000 pixels.
fn huge_bmp() -> Vec<u8> {
    let mut bmp = vec![0; 54];
    bmp[..2].copy_from_slice(b"BM");
    bmp[10..14].copy_from_slice(&54_u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40_u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&50_000_u32.to_le_bytes());
    bmp[22..26].copy_from_slice(&50_000_u32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1_u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24_u16.to_le_bytes());
    bmp
}

#[test]
fn pictures_are_checked_when_their_page_shows() {
    let mut entries = epub_fixture();
    replace(
        &mut entries,
        "OPS/book.opf",
        "</manifest>",
        r#"<item id="corrupt" href="text/corrupt.png" media-type="image/png"/><item id="huge" href="text/huge.bmp" media-type="image/bmp"/></manifest>"#,
    );
    replace(&mut entries, "OPS/text/z.xhtml", "<body>", r#"<body><img src="corrupt.png"/><img src="huge.bmp"/>"#);
    entries.extend([entry("OPS/text/corrupt.png", "not an image"), entry("OPS/text/huge.bmp", huge_bmp())]);
    let book = open_epub(&entries).unwrap();
    assert!(book.page(0, &cancel()).is_err() && book.encoded(0, &cancel()).is_err(), "served a corrupt picture");
    for error in [book.page(1, &cancel()).unwrap_err(), book.encoded(1, &cancel()).unwrap_err()] {
        assert!(error.to_string().contains("decoded pixels"), "{error}");
    }
    for (media, want) in [("image/tiff", "unsupported image"), ("image/png", "missing resource")] {
        let mut entries = epub_fixture();
        replace(&mut entries, "OPS/book.opf", "</manifest>", &format!(r#"<item id="p" href="text/p.img" media-type="{media}"/></manifest>"#));
        replace(&mut entries, "OPS/text/z.xhtml", "<body>", r#"<body><img src="p.img"/>"#);
        let error = open_epub(&entries).err().unwrap().to_string();
        assert!(error.contains(want), "{error}");
    }
}

#[test]
fn headings_lead_to_their_first_text_page() {
    let img = r#"<img src="../images/panel"/>"#;
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="../images/panel"/></svg>"#;
    for (heading, page) in [
        (format!("{img}First heading"), 1),
        (format!("{svg}First heading"), 1),
        (format!("First heading{img} continued"), 0),
        (format!("\u{ad}{img}First heading"), 1),
    ] {
        let mut entries = epub_fixture();
        replace(&mut entries, "OPS/book.opf", "</manifest>", r#"<item id="panel" href="images/panel" media-type="image/png"/></manifest>"#);
        replace(&mut entries, "OPS/text/z.xhtml", "<h1>First heading</h1>", &format!("<h1>{heading}</h1>{img}"));
        replace(&mut entries, "OPS/text/z.xhtml", "</body>", "<h2>Next heading</h2></body>");
        entries.push(entry("OPS/images/panel", png()));
        let book = open_epub(&entries).unwrap();
        let chapters = book.chapters(&cancel()).unwrap();
        assert_eq!(chapters.len(), 3, "{chapters:?}");
        assert_eq!(chapters[0].page, page, "{heading}: {chapters:?}");
        assert!(text(&book, page).starts_with("First heading"));
        assert_eq!(chapters[1..], [chapter("Next heading", book.len() - 2, 1), chapter("Notes", book.len() - 1, 0)]);
        assert_eq!(book.image_name(if page == 0 { 1 } else { 0 }), Some("OPS/images/panel"), "heading image order changed");
    }
}

#[test]
fn unsupported_markup_reads_as_its_fallback() {
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/book.opf", "</manifest>", r#"<item id="font" href="fonts/missing.otf" media-type="font/otf"/></manifest>"#);
    replace(
        &mut entries,
        "OPS/text/z.xhtml",
        "First heading",
        r#"Heading <epub:switch xmlns:epub="http://www.idpf.org/2007/ops"><epub:case required-namespace="urn:x">I</epub:case><epub:default>one</epub:default></epub:switch> <math xmlns="http://www.w3.org/1998/Math/MathML" alttext="x^2"><mi>x</mi><mn>2</mn></math>"#,
    );
    replace(
        &mut entries,
        "OPS/text/z.xhtml",
        "<p>Second paragraph.</p>",
        concat!(
            r#"<p>Energy <math xmlns="http://www.w3.org/1998/Math/MathML" alttext="E=mc^2"><mi>E</mi></math> and <math xmlns="http://www.w3.org/1998/Math/MathML"><mi>x</mi></math>.</p>"#,
            r#"<video src="https://cdn.example/v.mp4" poster="https://cdn.example/p.png"><p>Video fallback.</p></video><embed src="https://example.org/x"/><iframe src="https://example.org/embed"></iframe>"#,
            r#"<epub:switch xmlns:epub="http://www.idpf.org/2007/ops"><epub:case required-namespace="urn:x"><p>Case text.</p><img src="https://example.org/unused.png"/></epub:case><epub:default><p>Default text.</p></epub:default></epub:switch>"#,
            r#"<p>Word<o:p xmlns:o="urn:schemas-microsoft-com:office:office"> markup<o:switch><o:case>.</o:case></o:switch></o:p></p>"#,
        ),
    );
    let book = open_epub(&entries).unwrap();
    assert_eq!(text(&book, 0), "Heading one x^2\nHello reader & friends.\nEnergy E=mc^2 and x.\nVideo fallback.\nDefault text.\nWord markup.");
    assert_eq!(book.chapters(&cancel()).unwrap()[0].title, "Heading one x^2");
}

/// An EPUB whose first document starts with `body`, with `pictures` as
/// (name under `OPS/images/`, media type, bytes) in its manifest.
fn epub_with_pictures(body: &str, pictures: &[(&str, &str, Vec<u8>)]) -> yoyomi_core::Result<TextBook> {
    let mut entries = epub_fixture();
    for (name, media, bytes) in pictures {
        replace(&mut entries, "OPS/book.opf", "</manifest>", &format!(r#"<item id="{name}" href="images/{name}" media-type="{media}"/></manifest>"#));
        entries.push(entry(&format!("OPS/images/{name}"), bytes));
    }
    replace(&mut entries, "OPS/text/z.xhtml", "<body>", &format!("<body>{body}"));
    open_epub(&entries)
}

fn epub_with_a_png(body: &str) -> yoyomi_core::Result<TextBook> {
    epub_with_pictures(body, &[("a.png", "image/png", png())])
}

#[test]
fn an_image_with_srcset_shows_its_src() {
    let book = epub_with_a_png(r#"<img src="../images/a.png" srcset="../images/a-2x.png 2x, ../images/a.png 1x"/>"#).unwrap();
    assert_eq!(book.image_name(0), Some("OPS/images/a.png"));
    assert_eq!(book.page(0, &cancel()).unwrap().dimensions(), (3, 5));
}

#[test]
fn a_picture_element_shows_its_img_src() {
    let book = epub_with_a_png(r#"<picture><source srcset="../images/a.webp"/><img src="../images/a.png"/></picture>"#).unwrap();
    assert_eq!(book.image_name(0), Some("OPS/images/a.png"));
}

#[test]
fn an_image_with_only_srcset_lacks_src() {
    let error = epub_with_a_png(r#"<img srcset="../images/a.png 2x"/>"#).err().unwrap().to_string();
    assert!(error.contains("EPUB image lacks src"), "{error}");
}

#[test]
fn srcset_candidates_outside_the_archive_are_never_opened() {
    for candidate in ["../../x.png", "../../../x.png", "https://example.com/x.png"] {
        let book = epub_with_a_png(&format!(r#"<img src="../images/a.png" srcset="{candidate} 2x"/>"#)).unwrap();
        assert_eq!(book.image_name(0), Some("OPS/images/a.png"), "{candidate}");
        assert_eq!(book.page(0, &cancel()).unwrap().dimensions(), (3, 5));
    }
}

fn encode(picture: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    picture.into().write_to(&mut out, format).unwrap();
    out.into_inner()
}

#[test]
fn transparent_pictures_sit_on_white_paper() {
    let mut picture = RgbaImage::from_pixel(3, 1, Rgba([10, 20, 30, 255]));
    picture.put_pixel(1, 0, Rgba([50, 60, 70, 0]));
    picture.put_pixel(2, 0, Rgba([100, 0, 200, 128]));
    let png = encode(picture, ImageFormat::Png);
    let book = epub_with_pictures(r#"<img src="../images/t.png"/>"#, &[("t.png", "image/png", png.clone())]).unwrap();
    let want = [[10, 20, 30, 255], [255, 255, 255, 255], [177, 127, 227, 255]];
    assert_eq!(book.page(0, &cancel()).unwrap().pixels().map(|p| p.0).collect::<Vec<_>>(), want);
    // The browser reader lays the stored picture on white itself, which keeps animations.
    let encoded = book.encoded(0, &cancel()).unwrap();
    assert_eq!((&*encoded.bytes, encoded.mime), (png.as_slice(), "image/png"));
}

#[test]
fn opaque_pictures_come_as_stored() {
    let mut picture = RgbaImage::from_pixel(3, 1, Rgba([10, 20, 30, 255]));
    picture.put_pixel(1, 0, Rgba([200, 100, 0, 255]));
    let (png, jpeg) = (encode(picture.clone(), ImageFormat::Png), encode(DynamicImage::from(picture.clone()).into_rgb8(), ImageFormat::Jpeg));
    let pictures = [("o.png", "image/png", png.clone()), ("o.jpg", "image/jpeg", jpeg.clone())];
    let book = epub_with_pictures(r#"<img src="../images/o.png"/><img src="../images/o.jpg"/>"#, &pictures).unwrap();
    assert_eq!(*book.page(0, &cancel()).unwrap(), picture);
    let (encoded_png, encoded_jpeg) = (book.encoded(0, &cancel()).unwrap(), book.encoded(1, &cancel()).unwrap());
    assert_eq!((&*encoded_png.bytes, encoded_png.mime), (png.as_slice(), "image/png"));
    assert_eq!((&*encoded_jpeg.bytes, encoded_jpeg.mime), (jpeg.as_slice(), "image/jpeg"));
}

#[test]
fn pre_keeps_aligned_columns() {
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/text/z.xhtml", "<p>Second paragraph.</p>", "<pre>name   size\nab     1\tx</pre>");
    let text = open_epub(&entries).unwrap().page_text(0).unwrap();
    assert_eq!(text[2..], ["name\u{a0}\u{a0} size", "ab\u{a0}\u{a0}\u{a0}\u{a0} 1\u{a0}\u{a0}\u{a0} x"]);
}

#[test]
fn body_styles_lists_and_pre_keep_their_form() {
    let long = format!("{} end", "wrap ".repeat(30).trim());
    let mut entries = epub_fixture();
    replace(
        &mut entries,
        "OPS/text/z.xhtml",
        "<p>Second paragraph.</p>",
        &format!(
            "<p><i>Thought</i></p><h3>Sub</h3><ul><li>one</li><li><p>two</p></li><li><ruby>three<rt>san</rt></ruby></li></ul><pre>\nline one\n\tindented\n\nline two\n</pre><p><b>Loud</b></p><p>{long}</p>"
        ),
    );
    let book = open_epub(&entries).unwrap();
    assert_eq!(book.len(), 2, "h3 or lists changed pagination");
    let want = [
        "First heading",
        "Hello reader & friends.",
        "Thought",
        "Sub",
        "\u{2022} one",
        "\u{2022} two",
        "\u{2022} three",
        "line one",
        "\u{a0}\u{a0}\u{a0}\u{a0}indented",
        "\u{a0}",
        "line two",
        "Loud",
        long.as_str(),
    ];
    assert_eq!(book.page_text(0).unwrap(), want);
    assert_eq!(book.page_text(1).unwrap(), ["Last note."]);
    assert_eq!(book.page_text(2), None);
    assert_eq!(book.chapters(&cancel()).unwrap(), [chapter("First heading", 0, 0), chapter("Sub", 0, 2), chapter("Notes", 1, 0)]);
    for (line, face) in [(0, "Go-Bold"), (1, "GoRegular"), (2, "Go-Italic"), (11, "Go-Bold")] {
        assert_eq!(book.face(0, line, 0), face, "line {line} {:?}", book.page_lines(0)[line].text);
    }
    assert_eq!(book.face(0, 1, "Hello ".len()), "Go-Italic", "emphasis inside a paragraph");
    book.encoded(0, &cancel()).unwrap();
}

#[test]
fn link_labels_and_latin_text_normalize() {
    let mut entries = epub_fixture();
    replace(
        &mut entries,
        "OPS/text/z.xhtml",
        "<body>",
        "<body><p><a href=\"https://publisher.example/legal\">Publisher</a> co\u{ad}operate Cafe\u{301}</p>",
    );
    assert_eq!(text(&open_epub(&entries).unwrap(), 0), "Publisher cooperate Café");
}

fn span(start: usize, end: usize, target: Link) -> LinkSpan {
    LinkSpan { start, end, target }
}

#[test]
fn text_layers_carry_boxes_links_and_anchors() {
    let mut entries = epub_fixture();
    replace(
        &mut entries,
        "OPS/text/z.xhtml",
        "<p>Second paragraph.</p>",
        r#"<p><a href="https://example.com/a?utm_source=x">Second</a> <a href="a.xhtml#note">paragraph</a>.</p><span id="ztail"/>"#,
    );
    let words = "word ".repeat(300);
    replace(
        &mut entries,
        "OPS/text/a.xhtml",
        "<p>Last note.</p>",
        &format!(
            r#"<p id="note">Last <a href="z.xhtml#missing">note</a> <a href="javascript:alert(1)">x</a>.</p><ul><li><a href="https://example.com/li">item</a></li><li><a href="https://example.com/block"><p>Block</p></a></li></ul><p>{words}<span id="deep"/>tail</p><span id="tail"/>"#
        ),
    );
    let book = open_epub(&entries).unwrap();
    let layer = book.text_layer(0, &cancel()).unwrap().unwrap();
    assert!(Arc::ptr_eq(&layer, &book.text_layer(0, &cancel()).unwrap().unwrap()), "the text layer was built again");
    let last = layer.lines.last().unwrap();
    assert_eq!(last.text, "Second paragraph.");
    assert_eq!(last.links, [span(0, 6, Link::Url("https://example.com/a?utm_source=x".into())), span(7, 16, Link::Page(1))]);
    let margin = f64::from(MARGIN) / f64::from(PAGE_WIDTH);
    for line in &layer.lines {
        assert!(0.0 < line.top && line.top < line.bottom && line.bottom < 1.0, "{line:?}");
        assert!(line.boxes.iter().all(|b| b.x0 >= margin - 1e-9 && b.x0 <= b.x1 && b.x1 <= 1.0), "{line:?}");
        assert_eq!(line.boxes.iter().map(|b| b.count).sum::<usize>(), line.text.chars().count(), "{line:?}");
    }
    let notes = book.text_layer(1, &cancel()).unwrap().unwrap();
    assert_eq!(notes.lines[0].text, "Last note x.");
    assert_eq!(notes.lines[0].links, [span(5, 9, Link::Page(0))], "a missing fragment leads to its document; scripts lead nowhere");
    assert_eq!(notes.lines[1].text, "\u{2022} item");
    assert_eq!(notes.lines[1].links, [span(2, 6, Link::Url("https://example.com/li".into()))]);
    assert_eq!(notes.lines[2].text, "\u{2022} Block");
    assert_eq!(book.page_lines(1)[2].links, [LinkRange { start: 2, end: 7, href: "https://example.com/block".into(), internal: false }]);
    let want: HashMap<String, usize> = [
        ("OPS/text/z.xhtml", 0),
        ("OPS/text/z.xhtml#ztail", 0),
        ("OPS/text/a.xhtml", 1),
        ("OPS/text/a.xhtml#note", 1),
        ("OPS/text/a.xhtml#deep", 2),
        ("OPS/text/a.xhtml#tail", 2),
    ]
    .into_iter()
    .map(|(key, page)| (key.to_owned(), page))
    .collect();
    assert_eq!((book.len(), book.anchors()), (3, &want));
    assert!(book.text_layer(3, &cancel()).unwrap().is_none());
}

#[test]
fn the_text_layer_cache_is_bounded() {
    let mut entries = epub_fixture();
    let pages: String = (0..65).map(|i| format!("<h1>Page {i}</h1>")).collect();
    replace(&mut entries, "OPS/text/z.xhtml", "<h1>First heading</h1>", &pages);
    let book = open_epub(&entries).unwrap();
    for page in 0..65 {
        book.text_layer(page, &cancel()).unwrap().unwrap();
    }
    let cached = book.cached_layers();
    assert_eq!(cached.len(), 64);
    assert!(!cached.contains(&0), "the oldest layer stayed");
    book.text_layer(0, &cancel()).unwrap().unwrap();
    let cached = book.cached_layers();
    assert!(cached.contains(&0) && !cached.contains(&1) && cached.len() == 64);
}

#[test]
fn ruby_readings_sit_above_their_base() {
    for base in ["word".to_owned(), "b".repeat(61), "base ".repeat(600)] {
        let mut entries = epub_fixture();
        replace(
            &mut entries,
            "OPS/text/z.xhtml",
            "Hello <em>reader</em> &amp; friends.",
            &format!("Before <ruby>{base}<rp>(</rp><rt>reading annotation</rt><rp>)</rp></ruby> after."),
        );
        let book = open_epub(&entries).unwrap();
        let (mut all, mut readings, mut inline) = (String::new(), String::new(), false);
        for page in (0..book.len()).filter(|&page| book.kind(page) == PageKind::Text) {
            for line in book.page_lines(page) {
                all.push_str(&line.text);
                inline |= line.text.contains("Before ") && line.text.contains("word");
                for reading in &line.annotations {
                    readings.push_str(&reading.text);
                    assert!(reading.top >= MARGIN && reading.bottom < line.top, "ruby outside its band");
                }
                assert!(line.bottom <= PAGE_HEIGHT as i32 - MARGIN, "base clipped at the page bottom");
            }
        }
        let want = format!("Before {} after.", base.trim_end());
        assert!(all.contains(&want) && !all.contains(['(', ')']), "{all:?}");
        assert_eq!(readings, "reading annotation");
        assert!(inline || base != "word", "ruby broke the inline flow");
        book.encoded(0, &cancel()).unwrap();
    }
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/text/z.xhtml", "First heading", "<ruby>Base<rp>(</rp><rt>reading</rt><rp>)</rp></ruby>");
    assert_eq!(open_epub(&entries).unwrap().chapters(&cancel()).unwrap()[0].title, "Base");
}

#[test]
fn a_long_heading_leads_to_its_first_page() {
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/text/z.xhtml", "First heading", &"Long heading ".repeat(500));
    let book = open_epub(&entries).unwrap();
    assert!(book.len() >= 3);
    assert_eq!(book.chapters(&cancel()).unwrap()[0].page, 0);
}

#[test]
fn navigation_documents_win_over_headings() {
    const NAV: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="landmarks"><ol><li><a href="text/z.xhtml">Wrong list</a></li></ol></nav><nav epub:type="toc"><ol><li><span>Part</span><ol><li><a href="text/z.xhtml#first%3Aid">Navigation title</a></li><li><a href="text/z.xhtml#deep">Deep target</a></li></ol></li><li><a href="text/a.xhtml">Afterword</a></li><li><a href="text/z.xhtml#missing">Broken</a></li><li><a href="https://example.com/">Web</a></li><li><a href="../../../outside">Outside</a></li></ol></nav></body></html>"#;
    const NCX: &str = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap><navPoint><navLabel><text>NCX title</text></navLabel><content src="text/z.xhtml#first%3Aid"/><navPoint><navLabel><text>NCX nested</text></navLabel><content src="text/a.xhtml"/></navPoint></navPoint></navMap></ncx>"#;
    for source in ["nav", "ncx", "both", "broken nav", "broken both", "invalid targets"] {
        let mut entries = epub_fixture();
        replace(&mut entries, "OPS/text/z.xhtml", "<h1>", r#"<h1 id="first:id">"#);
        replace(&mut entries, "OPS/text/z.xhtml", "</body>", &format!(r#"<p>{}<span id="deep"/>Tail</p></body>"#, "word ".repeat(800)));
        let nav = match source {
            "broken nav" | "broken both" => "<html>".to_owned(),
            "invalid targets" => NAV.replace("text/", "missing/"),
            _ => NAV.to_owned(),
        };
        let ncx = if source == "broken both" { "<ncx>" } else { NCX };
        if source != "ncx" {
            replace(
                &mut entries,
                "OPS/book.opf",
                "</manifest>",
                r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="scripted nav"/></manifest>"#,
            );
            entries.push(entry("OPS/nav.xhtml", nav));
        }
        if source != "nav" && source != "invalid targets" {
            replace(
                &mut entries,
                "OPS/book.opf",
                "</manifest>",
                r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/></manifest>"#,
            );
            replace(&mut entries, "OPS/book.opf", "<spine>", r#"<spine toc="ncx">"#);
            entries.push(entry("OPS/toc.ncx", ncx));
        }
        let book = open_epub(&entries).unwrap();
        let (deep, last) = (book.anchors()["OPS/text/z.xhtml#deep"], book.len() - 1);
        assert!(deep > 0, "the fixture did not paginate the fragment target");
        let want = match source {
            "ncx" | "broken nav" => vec![chapter("NCX title", 0, 0), chapter("NCX nested", last, 1)],
            "broken both" | "invalid targets" => vec![chapter("First heading", 0, 0), chapter("Notes", last, 0)],
            _ => vec![chapter("Part", 0, 0), chapter("Navigation title", 0, 1), chapter("Deep target", deep, 1), chapter("Afterword", last, 0)],
        };
        assert_eq!(book.chapters(&cancel()).unwrap(), want, "{source}");
    }
}

#[test]
fn navigation_reaches_picture_spine_items() {
    let mut entries = epub_fixture();
    replace(&mut entries, "OPS/book.opf", r#"href="text/z.xhtml" media-type="application/xhtml+xml""#, r#"href="panel.png" media-type="image/png""#);
    replace(
        &mut entries,
        "OPS/book.opf",
        "</manifest>",
        r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>"#,
    );
    entries.push(entry("OPS/panel.png", png()));
    entries.push(entry("OPS/nav.xhtml", r#"<html><body><nav role="doc-toc"><ol><li><a href="panel.png">Cover</a></li></ol></nav></body></html>"#));
    assert_eq!(open_epub(&entries).unwrap().chapters(&cancel()).unwrap(), [chapter("Cover", 0, 0)]);

    let mut entries = epub_fixture();
    replace(
        &mut entries,
        "OPS/book.opf",
        r#"href="text/z.xhtml" media-type="application/xhtml+xml""#,
        r#"href="wrapper.svg" media-type="image/svg+xml""#,
    );
    replace(
        &mut entries,
        "OPS/book.opf",
        "</manifest>",
        r#"<item id="panel" href="panel.png" media-type="image/png"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>"#,
    );
    entries.push(entry("OPS/panel.png", png()));
    entries.push(entry("OPS/wrapper.svg", r#"<svg xmlns="http://www.w3.org/2000/svg"><image id="cover" href="panel.png"/></svg>"#));
    entries.push(entry(
        "OPS/nav.xhtml",
        r#"<html><body><nav role="doc-toc"><ol><li><a href="wrapper.svg#cover">Cover</a></li></ol></nav></body></html>"#,
    ));
    assert_eq!(open_epub(&entries).unwrap().chapters(&cancel()).unwrap(), [chapter("Cover", 0, 0)]);
}

fn fb2_fixture() -> String {
    format!(
        r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0" xmlns:l="http://www.w3.org/1999/xlink">
<description><title-info><book-title>Fiction Title</book-title><coverpage><image l:href="#cover"/></coverpage></title-info></description>
<body>
<title><p>Fiction Title</p></title>
<section><title><p>Chapter One</p></title>
<p>Hello <emphasis>reader</emphasis> &amp; <a l:href="#n1">friends</a>.</p>
<empty-line/>
<subtitle>* * *</subtitle>
<poem><title><p>Ode</p></title><stanza><v>First verse</v><v>Second verse</v></stanza></poem>
<image l:href="#missing"/>
<image l:href="#vector"/>
<image l:href="#cover"/>
<p>After the picture.</p>
</section>
</body>
<body name="notes"><section id="n1"><title><p>1</p></title><p>A note.</p></section></body>
<binary id="cover" content-type="image/png">{}</binary>
<binary id="broken" content-type="image/png">not base64!</binary>
<binary id="vector" content-type="image/svg+xml">PHN2Zy8+</binary>
</FictionBook>"##,
        STANDARD.encode(png())
    )
}

fn check_fb2(book: &TextBook, path: &Path, greeting: &str) {
    assert_eq!((book.title(), book.len(), book.path()), ("Fiction Title", 5, path));
    for page in [0, 3] {
        assert_eq!(
            (book.kind(page), book.can_invert(page), book.image_name(page)),
            (PageKind::Image, false, Some("image-0")),
            "page {page} is not the cover"
        );
        assert_eq!(book.page(page, &cancel()).unwrap().width(), 3);
    }
    assert_eq!(text(book, 1), "Fiction Title");
    let greeting_line = format!("{greeting} reader & friends.");
    assert_eq!(book.page_text(2).unwrap(), ["Chapter One", greeting_line.as_str(), "* * *", "Ode", "First verse", "Second verse"]);
    assert_eq!(book.page_text(4).unwrap(), ["After the picture.", "1", "A note."]);
    assert_eq!(book.chapters(&cancel()).unwrap(), [chapter("Fiction Title", 1, 0), chapter("Chapter One", 2, 0)]);
    for (line, face) in [(0, "Go-Bold"), (1, "GoRegular"), (2, "Go-Bold"), (3, "Go-Bold"), (4, "GoRegular")] {
        assert_eq!(book.face(2, line, 0), face, "line {line}");
    }
    assert_eq!(book.face(2, 1, greeting.chars().count() + 1), "Go-Italic", "emphasis inside a paragraph");
    let start = greeting.chars().count() + " reader & ".len();
    assert_eq!(book.page_lines(2)[1].links, [LinkRange { start, end: start + "friends".len(), href: "#n1".into(), internal: true }]);
    assert_eq!(book.anchors()["#n1"], 4);
}

#[test]
fn fb2_opens_plain_zipped_and_in_legacy_charsets() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.fb2");
    std::fs::write(&plain, format!("\u{feff}{}", fb2_fixture())).unwrap();
    check_fb2(&fb2::open_file(&plain).unwrap(), &plain, "Hello");

    let (_zip_dir, zipped) = write_zip(&[entry("book.fb2", fb2_fixture())], ".fb2.zip");
    check_fb2(&fb2::open_zip(&zipped, "book.fb2").unwrap(), &zipped, "Hello");
    assert_eq!(open_any(&zipped).len(), 5);

    let legacy = fb2_fixture().replacen(r#"encoding="utf-8""#, r#"encoding="windows-1251""#, 1).replacen("Hello", "Привет", 1);
    let path = dir.path().join("legacy.fb2");
    std::fs::write(&path, encoding_rs::WINDOWS_1251.encode(&legacy).0).unwrap();
    check_fb2(&fb2::open_file(&path).unwrap(), &path, "Привет");
    assert_eq!(open_any(&path).len(), 5);

    for (name, little, label) in [("wide-le.fb2", true, "UTF-16"), ("wide-be.fb2", false, "utf-16be")] {
        let wide = fb2_fixture().replacen(r#"encoding="utf-8""#, &format!(r#"encoding="{label}""#), 1);
        let mut bytes = if little { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
        bytes.extend(wide.encode_utf16().flat_map(|unit| if little { unit.to_le_bytes() } else { unit.to_be_bytes() }));
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        check_fb2(&fb2::open_file(&path).unwrap(), &path, "Hello");
    }
}

#[test]
fn fb2_sections_nest_chapters() {
    let data = b"<FictionBook><body><section><title><p>First</p></title><p>Text</p><section><title><p>Nested</p></title><p>More text</p></section></section></body></FictionBook>";
    let book = fb2::open(data, Path::new("/books/book.fb2")).unwrap();
    assert_eq!(book.chapters(&cancel()).unwrap(), [chapter("First", 0, 0), chapter("Nested", 1, 1)]);
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

fn docx_fixture() -> Entries {
    vec![
        entry("[Content_Types].xml", r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#),
        entry(
            "word/document.xml",
            format!(
                r#"<w:document {W} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><w:body>
<w:p><w:pPr><w:pStyle w:val="Ttulo1"/></w:pPr><w:r><w:t>Heading One</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">Plain </w:t></w:r><w:r><w:rPr><w:b/><w:i w:val="0"/></w:rPr><w:t>bold</w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> PAGEREF _Toc1 </w:instrText></w:r><w:r><w:tab/><w:t>text</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>item</w:t></w:r></w:p>
<w:p><w:bookmarkStart w:id="0" w:name="_Toc1"/><w:hyperlink r:id="rId3"><w:r><w:rPr><w:i/></w:rPr><w:t>Italic</w:t></w:r></w:hyperlink><w:bookmarkEnd w:id="0"/></w:p>
<w:p><w:r><w:drawing><a:blip r:embed="rId2"/></w:drawing></w:r></w:p>
<w:p><w:r><w:drawing><a:blip r:embed="rId9"/></w:drawing></w:r><w:r><w:t>After</w:t></w:r></w:p>
<w:p><w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:t>Choice</w:t></mc:Choice><mc:Choice Requires="w14"><w:t>Twice</w:t></mc:Choice><mc:Fallback><w:t>Fallback</w:t></mc:Fallback></mc:AlternateContent></w:r></w:p>
<w:p><w:r><mc:AlternateContent><mc:Fallback><w:t>Only fallback</w:t></mc:Fallback></mc:AlternateContent></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:sectPr/></w:body></w:document>"#
            ),
        ),
        entry(
            "word/styles.xml",
            format!(
                r#"<w:styles {W}><w:style w:styleId="Ttulo1"><w:name w:val="heading 1"/></w:style><w:style w:styleId="Normal"><w:name w:val="Normal"/></w:style></w:styles>"#
            ),
        ),
        entry(
            "word/_rels/document.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/" TargetMode="External"/></Relationships>"#,
        ),
        entry("word/media/image1.png", png()),
    ]
}

fn open_docx(entries: &[Entry]) -> TextBook {
    let (_dir, path) = write_zip(entries, ".docx");
    docx::open(&path).unwrap()
}

#[test]
fn docx_reads_headings_runs_lists_pictures_and_tables() {
    let book = open_docx(&docx_fixture());
    assert_eq!((book.title(), book.len()), ("book", 3));
    assert_eq!(book.page_text(0).unwrap(), ["Heading One", "Plain bold text", "\u{2022} item", "Italic"]);
    assert_eq!((book.kind(1), book.can_invert(1), book.image_name(1)), (PageKind::Image, false, Some("word/media/image1.png")));
    assert_eq!(book.page(1, &cancel()).unwrap().width(), 3);
    assert_eq!(book.page_text(2).unwrap(), ["After", "Fallback", "Only fallback", "Cell"]);
    assert_eq!(book.chapters(&cancel()).unwrap(), [chapter("Heading One", 0, 0)]);
    for (line, rune, face) in [(0, 0, "Go-Bold"), (1, 0, "GoRegular"), (1, 6, "Go-Bold"), (1, 11, "GoRegular"), (3, 0, "Go-Italic")] {
        assert_eq!(book.face(0, line, rune), face, "line {line} rune {rune}");
    }
    assert_eq!(book.anchors()["#_Toc1"], 0);
    assert_eq!(book.page_lines(0)[3].links, [LinkRange { start: 0, end: 6, href: "https://example.org/".into(), internal: false }]);
    let (_dir, renamed) = write_zip(&docx_fixture(), ".epub");
    assert_eq!(open_any(&renamed).len(), 3, "DOCX contents must win over the extension");
}

fn docx_body(body: &str) -> Entries {
    let mut entries = docx_fixture();
    entries[1].1 = format!(
        r#"<w:document {W} xmlns:word="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:future="urn:unsupported" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>{body}</w:body></w:document>"#
    )
    .into_bytes();
    entries
}

#[test]
fn docx_markup_compatibility_picks_a_supported_choice() {
    const FALLBACK: &str = "<mc:Fallback><w:t>Fallback</w:t></mc:Fallback>";
    let cases = [
        (format!(r#"<mc:Choice Requires="future"><w:t>Wrong</w:t></mc:Choice>{FALLBACK}"#), "Fallback"),
        (format!(r#"<mc:Choice Requires="missing"><w:t>Wrong</w:t></mc:Choice>{FALLBACK}"#), "Fallback"),
        (format!(r#"<mc:Choice><w:t>Wrong</w:t></mc:Choice>{FALLBACK}"#), "Fallback"),
        (format!(r#"<mc:Choice Requires="word future"><w:t>Wrong</w:t></mc:Choice>{FALLBACK}"#), "Fallback"),
        (
            format!(
                r#"<mc:Choice Requires="future"><w:t>Wrong</w:t></mc:Choice><mc:Choice Requires="word"><w:t>Chosen</w:t></mc:Choice><mc:Choice Requires="w"><w:t>Twice</w:t></mc:Choice>{FALLBACK}"#
            ),
            "Chosen",
        ),
        (format!(r#"<mc:Choice xmlns:local="http://schemas.openxmlformats.org/wordprocessingml/2006/main" Requires="local"><w:t>Chosen</w:t></mc:Choice>{FALLBACK}"#), "Chosen"),
        (format!(r#"<mc:Choice xmlns:word="urn:unsupported" Requires="word"><w:t>Wrong</w:t></mc:Choice>{FALLBACK}"#), "Fallback"),
        (r#"<mc:Choice Requires="word"><w:t>Chosen</w:t></mc:Choice>"#.to_owned(), "Chosen"),
        (r#"<mc:Choice Requires="future"><w:drawing/></mc:Choice><mc:Fallback><w:drawing><a:blip r:embed="rId2"/></w:drawing></mc:Fallback>"#.to_owned(), ""),
        (
            r#"<mc:Choice Requires="word"><w:drawing><mc:AlternateContent><mc:Choice Requires="future"><a:blip r:embed="absent"/></mc:Choice><mc:Fallback><a:blip r:embed="rId2"/></mc:Fallback></mc:AlternateContent></w:drawing></mc:Choice>"#
                .to_owned(),
            "",
        ),
    ];
    for (markup, want) in cases {
        let book = open_docx(&docx_body(&format!("<w:p><w:r><mc:AlternateContent>{markup}</mc:AlternateContent></w:r></w:p>")));
        assert_eq!(book.len(), 1, "{markup}");
        if want.is_empty() {
            assert_eq!(book.image_name(0), Some("word/media/image1.png"), "{markup}");
        } else {
            assert_eq!(book.page_text(0).unwrap().concat(), want, "{markup}");
        }
    }
}

#[test]
fn docx_picture_groups_give_each_picture_a_page() {
    let mut entries = docx_body(
        r#"<w:p><w:r><w:drawing><a:grpSp><a:pic><a:blip r:embed="rId2"/></a:pic><a:pic><a:blip r:embed="rId4"/></a:pic></a:grpSp></w:drawing></w:r></w:p>"#,
    );
    replace(
        &mut entries,
        "word/_rels/document.xml.rels",
        "</Relationships>",
        r#"<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image2.png"/></Relationships>"#,
    );
    entries.push(entry("word/media/image2.png", png()));
    let book = open_docx(&entries);
    assert_eq!(book.len(), 2);
    assert_eq!((book.image_name(0), book.image_name(1)), (Some("word/media/image1.png"), Some("word/media/image2.png")));
}

#[test]
fn docx_text_boxes_keep_their_paragraphs() {
    const BOX: &str = "<w:txbxContent><w:p><w:r><w:t>Boxed</w:t></w:r></w:p><w:p><w:r><w:t>Second</w:t></w:r></w:p></w:txbxContent>";
    let book = open_docx(&docx_body(&format!(
        "<w:p><w:r><w:drawing><a:graphic><a:graphicData><wps:wsp><wps:txbx>{BOX}</wps:txbx></wps:wsp></a:graphicData></a:graphic></w:drawing></w:r></w:p>"
    )));
    assert_eq!((book.len(), book.page_text(0).unwrap()), (1, vec!["Boxed".to_owned(), "Second".to_owned()]));
    let book = open_docx(&docx_body(&format!(
        r#"<w:p><w:r><w:pict><v:shape><v:textbox>{BOX}</v:textbox></v:shape><v:shape><v:imagedata r:id="rId2"/></v:shape></w:pict></w:r></w:p>"#
    )));
    assert_eq!((book.len(), book.page_text(0).unwrap()), (2, vec!["Boxed".to_owned(), "Second".to_owned()]));
    assert_eq!(book.image_name(1), Some("word/media/image1.png"));
}

#[test]
fn docx_ignores_xml_indentation() {
    let mut entries = docx_fixture();
    entries[1].1 = format!(
        "<w:document {W}>\n  <w:body>\n    <w:p>\n      <w:r><w:t>Hello</w:t></w:r>\n      <w:r><w:t>World</w:t></w:r>\n      <w:r><w:t xml:space=\"preserve\"> </w:t></w:r>\n      <w:r><w:t>again</w:t></w:r>\n    </w:p>\n  </w:body>\n</w:document>"
    )
    .into_bytes();
    assert_eq!(open_docx(&entries).page_text(0).unwrap(), ["HelloWorld again"]);
}

#[test]
fn docx_outline_levels_and_inherited_styles_make_headings() {
    let mut entries = docx_body(concat!(
        r#"<w:p><w:pPr><w:pStyle w:val="Custom"/></w:pPr><w:r><w:t>Inherited</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:outlineLvl w:val="8"/></w:pPr><w:r><w:t>Level nine</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Custom"/><w:outlineLvl w:val="9"/></w:pPr><w:r><w:t>Not a heading</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Cycle"/></w:pPr><w:r><w:t>Cyclic style</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Heading7"/></w:pPr><w:r><w:t>Built-in fallback</w:t></w:r></w:p>"#,
    ));
    entries[2].1 = format!(
        r#"<w:styles {W}><w:style w:styleId="Custom"><w:name w:val="Custom heading"/><w:basedOn w:val="Base"/></w:style><w:style w:styleId="Base"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style><w:style w:styleId="Cycle"><w:basedOn w:val="Cycle"/></w:style></w:styles>"#
    )
    .into_bytes();
    let want = [chapter("Inherited", 0, 0), chapter("Level nine", 0, 8), chapter("Built-in fallback", 0, 6)];
    assert_eq!(open_docx(&entries).chapters(&cancel()).unwrap(), want);

    for (level, direct, want) in
        [("4", "", vec![chapter("Heading", 0, 4)]), ("9", "", vec![]), ("4", r#"<w:outlineLvl w:val="1"/>"#, vec![chapter("Heading", 0, 1)])]
    {
        let mut entries = docx_body(&format!(r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/>{direct}</w:pPr><w:r><w:t>Heading</w:t></w:r></w:p>"#));
        entries[2].1 = format!(
            r#"<w:styles {W}><w:style w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Base"/></w:style><w:style w:styleId="Base"><w:pPr><w:outlineLvl w:val="{level}"/></w:pPr></w:style></w:styles>"#
        )
        .into_bytes();
        assert_eq!(open_docx(&entries).chapters(&cancel()).unwrap(), want, "inherited level {level}, direct {direct:?}");
    }
}

#[test]
fn text_books_open_through_formats() {
    let (_dir, path) = write_zip(&epub_fixture(), ".epub");
    let book = open_any(&path);
    assert_eq!((book.title(), book.len(), book.kind(0)), ("A Text Book", 2, PageKind::Text));
    assert_eq!(book.page_text(1).unwrap(), ["Last note."]);
    book.close();
    assert!(book.page(0, &cancel()).is_err(), "a closed book drew a page");
}

/// A file that counts the reads reaching it, each one a round trip on a
/// share.
struct CountingFile {
    file: File,
    reads: Rc<Cell<usize>>,
}

impl Read for CountingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads.set(self.reads.get() + 1);
        self.file.read(buf)
    }
}

impl Seek for CountingFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

#[test]
fn book_archives_reach_the_file_in_large_reads() {
    // An incompressible picture of 3 MiB behind 500 small documents.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let picture: Vec<u8> = (0..3 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let mut entries: Entries = (0..500).map(|i| entry(&format!("OPS/text/{i:03}.xhtml"), [i as u8; 100])).collect();
    entries.push(entry("OPS/picture.png", &picture));
    let (_dir, path) = write_zip(&entries, ".epub");

    let reads = Rc::new(Cell::new(0));
    let mut zip = zip_archive(CountingFile { file: File::open(&path).unwrap(), reads: reads.clone() }).unwrap();
    assert_eq!(zip.len(), 501);
    assert!(reads.get() <= 4, "opening took {} reads", reads.get());
    for (index, data, most) in [(250, &[250u8; 100][..], 2), (500, &picture[..], 5)] {
        reads.set(0);
        assert_eq!(read_bounded(zip_entry(&mut zip, index).unwrap(), 4 << 20).unwrap(), data, "entry {index}");
        assert!(reads.get() <= most, "entry {index} took {} reads", reads.get());
    }
}
