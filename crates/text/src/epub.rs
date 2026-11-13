//! EPUB 2 and 3: the container, the package, the spine in reading order and
//! the navigation document or NCX.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use yoyomi_core::{Chapter, Error, Result};

use crate::book::TextBook;
use crate::layout::{Layout, MAX_EPUB_DOCUMENT_BYTES, MAX_METADATA_BYTES, Package, Resource, Source, invalid, zip_archive};
use crate::path::{clean, link_target, resolve};
use crate::xml::{Node, OPS};

const MAX_ENTRIES: usize = 10_000;
const CONTAINER: &str = "urn:oasis:names:tc:opendocument:xmlns:container";
const OPF: &str = "http://www.idpf.org/2007/opf";
const NCX_MEDIA: &str = "application/x-dtbncx+xml";

/// A manifest item: its package name and media type.
#[derive(Clone)]
struct Item {
    name: String,
    media: String,
}

/// Junk entries: AppleDouble forks, hidden files and `__MACOSX` folders.
fn is_hidden(name: &str) -> bool {
    name.contains("__MACOSX/") || name.rsplit('/').next().is_some_and(|base| base.starts_with('.'))
}

pub(crate) fn open(path: &Path) -> Result<TextBook> {
    let zip = zip_archive(File::open(path)?).map_err(|e| invalid(format!("EPUB: {e}")))?;
    if zip.len() > MAX_ENTRIES {
        return Err(Error::Limit(format!("EPUB exceeds {MAX_ENTRIES} archive entries")));
    }
    let names: Vec<String> =
        zip.file_names().map(|name| name.map(|name| name.into_owned())).collect::<Result<_, _>>().map_err(|e| invalid(format!("EPUB: {e}")))?;
    let mut pkg = Package::default();
    for (index, name) in names.into_iter().enumerate() {
        if name.ends_with('/') || is_hidden(&name) {
            continue;
        }
        if name.is_empty()
            || clean(&name) != name
            || name.starts_with('/')
            || name == ".."
            || name.starts_with("../")
            || name.contains(['\\', '\0', ':'])
        {
            return Err(invalid(format!("EPUB unsafe archive path: {name:?}")));
        }
        if pkg.files.contains_key(&name) {
            return Err(invalid(format!("EPUB duplicate archive path: {name}")));
        }
        pkg.files.insert(name.clone(), Resource { name, source: Source::Zip(index) });
    }
    pkg.zip = Some(zip);
    encryption(&mut pkg)?;

    let container = pkg.document("META-INF/container.xml", MAX_METADATA_BYTES)?;
    if !container.is(CONTAINER, "container") {
        return Err(invalid("EPUB invalid container namespace or root"));
    }
    let rootfile = container
        .child("rootfiles")
        .children
        .iter()
        .find(|rf| rf.name.local == "rootfile" && rf.attr("media-type") == "application/oebps-package+xml");
    let package = resolve("", rootfile.ok_or_else(|| invalid("EPUB has no package rootfile"))?.attr("full-path")).map_err(invalid)?;
    let opf = pkg.document(&package, MAX_METADATA_BYTES)?;
    if !opf.is(OPF, "package") {
        return Err(invalid("EPUB invalid package namespace or root"));
    }
    let mut manifest: HashMap<String, Item> = HashMap::new();
    let mut resources: HashMap<String, String> = HashMap::new();
    for item in opf.child("manifest").children.iter().filter(|item| item.name.local == "item") {
        let (id, href, media) = (item.attr("id"), item.attr("href"), item.attr("media-type"));
        if id.is_empty() || href.is_empty() || media.is_empty() {
            return Err(invalid("EPUB manifest item lacks id, href or media-type"));
        }
        let name = resolve(&package, href).map_err(invalid)?;
        // A missing file fails only when the book reads it; unused fonts often go missing.
        if manifest.contains_key(id) || resources.contains_key(&name) {
            return Err(invalid(format!("EPUB duplicate manifest id or resource: {id}")));
        }
        manifest.insert(id.to_owned(), Item { name: name.clone(), media: media.to_owned() });
        resources.insert(name, media.to_owned());
    }

    let mut layout = Layout::new()?;
    let mut linear = false;
    for itemref in opf.child("spine").children.iter().filter(|r| r.name.local == "itemref") {
        let Item { name, media } = manifest
            .get(itemref.attr("idref"))
            .ok_or_else(|| invalid(format!("EPUB spine references missing manifest item: {}", itemref.attr("idref"))))?;
        match itemref.attr("linear") {
            "" | "yes" => linear = true,
            "no" => {}
            _ => return Err(invalid("EPUB invalid spine linear value")),
        }
        let first = layout.pages.len();
        layout.anchor(name, first);
        match media.as_str() {
            "application/xhtml+xml" | "image/svg+xml" => {
                let doc = pkg.document(name, MAX_EPUB_DOCUMENT_BYTES)?;
                layout.document(&mut pkg, name, &doc, &resources).map_err(|e| invalid(format!("EPUB {name}: {e}")))?;
            }
            media if media.starts_with("image/") => layout.add_image(&mut pkg, name, &resources)?,
            media => return Err(invalid(format!("EPUB unsupported spine media type: {media}"))),
        }
        layout.flush_page()?;
        if layout.pages.len() == first {
            return Err(invalid(format!("EPUB spine item has no readable content: {name}")));
        }
        layout.settle();
    }
    if !linear || layout.pages.is_empty() {
        return Err(invalid("EPUB needs a nonempty spine with linear content"));
    }
    let chapters = chapters(&mut pkg, &opf, &manifest, &layout);
    let title = opf.child("metadata").child("title").all_text();
    Ok(TextBook::new(path, title, layout, pkg.zip.take(), chapters))
}

/// Marks the files `META-INF/encryption.xml` lists; reading one fails.
fn encryption(pkg: &mut Package) -> Result<()> {
    if !pkg.files.contains_key("META-INF/encryption.xml") {
        return Ok(());
    }
    let doc = pkg.document("META-INF/encryption.xml", MAX_METADATA_BYTES)?;
    if doc.name.local != "encryption" {
        return Err(invalid("EPUB invalid encryption metadata"));
    }
    fn walk(n: &Node, pkg: &mut Package) -> Result<()> {
        if n.name.local == "EncryptedData" {
            let uri = n.child("CipherData").child("CipherReference").attr("URI");
            if uri.is_empty() {
                return Err(invalid("EPUB encryption metadata lacks resource URI"));
            }
            pkg.encrypted.insert(resolve("", uri).map_err(invalid)?);
        }
        n.children.iter().try_for_each(|c| walk(c, pkg))
    }
    walk(&doc, pkg)
}

/// The outline from EPUB 3 navigation, then the EPUB 2 NCX, then the
/// layout's headings.
fn chapters(pkg: &mut Package, opf: &Node, manifest: &HashMap<String, Item>, layout: &Layout) -> Result<Vec<Chapter>, String> {
    let navs = opf
        .child("manifest")
        .children
        .iter()
        .filter(|item| item.name.local == "item" && item.attr("properties").split_whitespace().any(|p| p == "nav"));
    let mut sources: Vec<Item> = navs.filter_map(|item| manifest.get(item.attr("id")).cloned()).collect();
    sources.extend(manifest.get(opf.child("spine").attr("toc")).cloned());
    let mut first_error = None;
    for Item { name, media } in sources {
        let ncx = media == NCX_MEDIA;
        let root = pkg.document(&name, MAX_METADATA_BYTES).and_then(|doc| {
            let root = if ncx { Some(doc.child("navMap")) } else { toc_nav(&doc) };
            root.filter(|root| !root.name.local.is_empty()).cloned().ok_or_else(|| invalid(format!("EPUB {name} has no table of contents")))
        });
        match root {
            Ok(root) => {
                let chapters = outline(&root, &name, 0, ncx, layout);
                if !chapters.is_empty() {
                    return Ok(chapters);
                }
            }
            Err(error) => {
                first_error.get_or_insert_with(|| error.to_string());
            }
        }
    }
    match first_error {
        Some(error) if layout.chapters.is_empty() => Err(error),
        _ => Ok(layout.chapters.clone()),
    }
}

/// The `nav` element of type `toc`.
fn toc_nav(n: &Node) -> Option<&Node> {
    let words = |value: &str, word: &str| value.split_whitespace().any(|w| w == word);
    let toc = n
        .attrs
        .iter()
        .any(|a| a.name.space == OPS && a.name.local == "type" && words(&a.value, "toc") || a.name.local == "role" && words(&a.value, "doc-toc"));
    if n.name.local == "nav" && toc {
        return Some(n);
    }
    n.children.iter().find_map(toc_nav)
}

/// The entries of a navigation list or NCX nav map whose targets lead to a
/// page. An entry without a target leads where its first child does.
fn outline(n: &Node, name: &str, depth: usize, ncx: bool, layout: &Layout) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    for c in &n.children {
        if c.name.local == "ol" && !ncx {
            chapters.extend(outline(c, name, depth, ncx, layout));
            continue;
        }
        if c.name.local != if ncx { "navPoint" } else { "li" } {
            continue;
        }
        let (mut label, mut href) = (c.child("a").all_text(), c.child("a").attr("href"));
        if ncx {
            (label, href) = (c.child("navLabel").all_text(), c.child("content").attr("src"));
        } else if label.is_empty() {
            label = c.child("span").all_text();
        }
        let children = outline(c, name, depth + 1, ncx, layout);
        // An unknown fragment must not jump to the document's first page.
        let page = match link_target(name, href) {
            Some((target, true)) => layout.anchors.get(&target).copied(),
            _ if href.is_empty() => children.first().map(|child| child.page),
            _ => None,
        };
        if let Some(page) = page.filter(|&page| !label.is_empty() && page < layout.pages.len()) {
            chapters.push(Chapter { title: label, page, depth });
        }
        chapters.extend(children);
    }
    chapters
}
