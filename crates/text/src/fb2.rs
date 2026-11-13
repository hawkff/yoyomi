//! FictionBook 2, plain or zipped, laid out through the XHTML layout:
//! elements map to XHTML, binaries become the pictures, and the cover comes
//! first, then every body in order, so notes follow the main text.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use yoyomi_core::Result;

use crate::book::TextBook;
use crate::layout::{
    DEPTH_ATTR, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_TOKENS, Package, Resource, Source, file_name, invalid, layout_body, read_bounded, zip_archive,
};
use crate::xml::{self, Node, XHTML};

pub(crate) fn open_file(path: &Path) -> Result<TextBook> {
    let data = read_bounded(File::open(path)?, MAX_DOCUMENT_BYTES).map_err(|e| invalid(format!("{}: {e}", file_name(path))))?;
    open(&data, path)
}

/// Opens the FB2 document at `entry` of the ZIP archive at `path`.
pub(crate) fn open_zip(path: &Path, entry: &str) -> Result<TextBook> {
    let mut zip = zip_archive(File::open(path)?).map_err(|e| invalid(format!("{}: {e}", file_name(path))))?;
    let file = zip.by_name(entry).map_err(|e| invalid(format!("{}: {entry}: {e}", file_name(path))))?;
    let data = read_bounded(file, MAX_DOCUMENT_BYTES).map_err(|e| invalid(format!("{}: {e}", file_name(path))))?;
    open(&data, path)
}

pub(crate) fn open(data: &[u8], path: &Path) -> Result<TextBook> {
    let file = file_name(path);
    let doc = xml::parse(data, MAX_DOCUMENT_TOKENS).map_err(|e| invalid(format!("{file}: {e}")))?;
    if doc.name.local != "FictionBook" {
        return Err(invalid(format!("{file}: not a FictionBook document")));
    }
    let info = doc.child("description").child("title-info");
    let mut pkg = Package::default();
    let mut resources = HashMap::new();
    let mut images = HashMap::new(); // binary id to package name
    for n in doc.children.iter().filter(|n| n.name.local == "binary") {
        let mut media = n.attr("content-type").to_lowercase();
        if media == "image/jpg" {
            media = "image/jpeg".to_owned();
        }
        let raster = media.strip_prefix("image/").is_some_and(|ext| matches!(ext, "jpeg" | "png" | "gif" | "webp" | "bmp"));
        let payload = STANDARD.decode(n.all_text().split_whitespace().collect::<String>());
        // References to a broken or non-raster binary drop with it.
        let (Ok(payload), true, false) = (payload, raster, n.attr("id").is_empty()) else { continue };
        let name = format!("image-{}", images.len());
        pkg.files.insert(name.clone(), Resource { name: name.clone(), source: Source::Memory(payload.into()) });
        resources.insert(name.clone(), media);
        images.insert(n.attr("id").to_owned(), name);
    }
    let mut body = Node::element(XHTML, "body");
    body.children.push(convert(info.child("coverpage"), "title-info", &images, false, 0));
    for n in doc.children.iter().filter(|n| n.name.local == "body") {
        body.children.push(convert(n, "FictionBook", &images, !n.attr("name").is_empty(), 0));
    }
    let layout = layout_body(&file, &mut pkg, body, &resources)?;
    let chapters = Ok(layout.chapters.clone());
    Ok(TextBook::new(path, info.child("book-title").all_text(), layout, None, chapters))
}

/// The XHTML element a FictionBook element becomes.
fn xhtml_name(local: &str) -> Option<&'static str> {
    Some(match local {
        "p" | "v" | "text-author" | "date" => "p",
        "body" | "section" | "poem" | "stanza" | "annotation" | "coverpage" | "table" => "div",
        "epigraph" | "cite" => "blockquote",
        "emphasis" => "em",
        "strong" => "strong",
        "empty-line" => "br",
        "a" => "a",
        "tr" => "tr",
        "th" => "th",
        "td" => "td",
        _ => return None,
    })
}

pub(crate) fn image_node(name: &str) -> Node {
    Node::element(XHTML, "img").with_attr("src", name)
}

/// Converts one FictionBook element. Section and body titles become
/// page-breaking headings; other titles and subtitles stay bold text. Ids
/// and link targets carry over.
fn convert(n: &Node, parent: &str, images: &HashMap<String, String>, notes: bool, depth: usize) -> Node {
    let local = n.name.local.as_str();
    if local.is_empty() {
        return Node::text(n.text.clone());
    }
    let depth = depth + usize::from(local == "section");
    let mut out = Node::element(XHTML, xhtml_name(local).unwrap_or("span"));
    out.attrs = n.attrs.iter().filter(|a| a.name.local == "id" || a.name.local == "href" && local == "a").cloned().collect();
    match local {
        "image" => {
            let href = n.attr("href");
            return images.get(href.strip_prefix('#').unwrap_or(href)).map_or_else(Node::default, |name| image_node(name));
        }
        "title" if !notes && (parent == "section" || parent == "body") => {
            out.name.local = "h2".to_owned();
            out = out.with_attr(DEPTH_ATTR, depth.saturating_sub(1).to_string());
        }
        "title" | "subtitle" => {
            out.name.local = "p".to_owned();
            let mut bold = Node::element(XHTML, "b");
            bold.children = n.children.iter().map(|c| convert(c, local, images, notes, depth)).collect();
            out.children = vec![bold];
            return out;
        }
        _ => {}
    }
    out.children = n.children.iter().map(|c| convert(c, local, images, notes, depth)).collect();
    out
}
