//! DOCX: `word/document.xml` laid out through the XHTML layout. Headings
//! come from outline levels and inherited styles; relationships supply
//! pictures and links.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use yoyomi_core::Result;

use crate::book::TextBook;
use crate::fb2::image_node;
use crate::layout::{
    DEPTH_ATTR, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_TOKENS, Package, Resource, Source, file_name, invalid, layout_body, read_bounded, zip_archive,
    zip_entry,
};
use crate::path::resolve;
use crate::xml::{self, MAX_DEPTH, Node, XHTML};

const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "webp", "bmp"];

/// Namespaces whose markup-compatibility choices the reader understands.
const KNOWN_NAMESPACES: [&str; 6] = [
    "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    "http://purl.oclc.org/ooxml/wordprocessingml/main",
    "http://schemas.openxmlformats.org/drawingml/2006/main",
    "http://purl.oclc.org/ooxml/drawingml/main",
    "http://schemas.openxmlformats.org/drawingml/2006/picture",
    "http://purl.oclc.org/ooxml/drawingml/picture",
];

/// What the conversion looks up.
struct Context<'a> {
    styles: HashMap<&'a str, &'a Node>,
    /// Relationship id to package name.
    images: HashMap<String, String>,
    /// Relationship id to external address.
    links: HashMap<String, String>,
}

pub(crate) fn open(path: &Path) -> Result<TextBook> {
    let file = file_name(path);
    let mut zip = zip_archive(File::open(path)?).map_err(|e| invalid(format!("{file}: {e}")))?;
    let names: HashMap<String, usize> = zip.file_names().enumerate().filter_map(|(i, name)| Some((name.ok()?.into_owned(), i))).collect();
    let mut read = |name: &str| -> Result<Node> {
        let index = *names.get(name).ok_or_else(|| invalid("missing archive entry"))?;
        let data = read_bounded(zip_entry(&mut zip, index)?, MAX_DOCUMENT_BYTES)?;
        xml::parse(&data, MAX_DOCUMENT_TOKENS).map_err(invalid)
    };
    let doc = read("word/document.xml").map_err(|e| invalid(format!("{file}: {e}")))?;
    let sheet = read("word/styles.xml").ok();
    let rels = read("word/_rels/document.xml.rels").ok();
    let mut pkg = Package::default();
    let mut resources = HashMap::new();
    let mut context = Context { styles: HashMap::new(), images: HashMap::new(), links: HashMap::new() };
    for style in sheet.iter().flat_map(|sheet| &sheet.children).filter(|s| s.name.local == "style") {
        context.styles.insert(style.attr("styleId"), style);
    }
    for r in rels.iter().flat_map(|rels| &rels.children).filter(|r| r.name.local == "Relationship") {
        let target = r.attr("Target");
        if r.attr("TargetMode") == "External" {
            context.links.insert(r.attr("Id").to_owned(), target.to_owned());
            continue;
        }
        let (base, target) = target.strip_prefix('/').map_or(("word/document.xml", target), |target| ("", target));
        let Ok(name) = resolve(base, target) else { continue };
        let extension = name.rsplit_once('.').map_or(String::new(), |(_, ext)| ext.to_ascii_lowercase());
        let (Some(&index), true) = (names.get(&name), IMAGE_EXTENSIONS.contains(&extension.as_str())) else { continue };
        let id = format!("image-{}", context.images.len());
        let media = if extension == "jpg" { "image/jpeg".to_owned() } else { format!("image/{extension}") };
        pkg.files.insert(id.clone(), Resource { name, source: Source::Zip(index) });
        resources.insert(id.clone(), media);
        context.images.insert(r.attr("Id").to_owned(), id);
    }
    let body = convert(doc.child("body"), &context, &mut vec![&doc]);
    drop(read);
    pkg.zip = Some(zip);
    let layout = layout_body(&file, &mut pkg, body, &resources)?;
    let chapters = Ok(layout.chapters.clone());
    Ok(TextBook::new(path, String::new(), layout, pkg.zip.take(), chapters))
}

/// The XHTML element a WordprocessingML element becomes.
fn xhtml_name(local: &str) -> &'static str {
    match local {
        "body" => "body",
        "p" => "p",
        "tbl" => "div",
        "tr" => "tr",
        "tc" => "td",
        "br" | "cr" => "br",
        "hyperlink" => "a",
        _ => "span",
    }
}

/// Converts one element; `ancestors` are the elements around it, for
/// namespace declarations.
fn convert<'a>(n: &'a Node, context: &Context<'_>, ancestors: &mut Vec<&'a Node>) -> Node {
    let local = n.name.local.as_str();
    if local.is_empty() {
        return Node::text(n.text.clone());
    }
    // Field codes and tracked deletions and moves do not show.
    if matches!(local, "instrText" | "delText" | "del" | "moveFrom") {
        return Node::default();
    }
    ancestors.push(n);
    let out = convert_element(n, context, ancestors);
    ancestors.pop();
    out
}

fn convert_element<'a>(n: &'a Node, context: &Context<'_>, ancestors: &mut Vec<&'a Node>) -> Node {
    let local = n.name.local.as_str();
    let mut out = Node::element(XHTML, xhtml_name(local));
    match local {
        "tab" => return Node::text(" "),
        "AlternateContent" => return convert(alternate(n, ancestors), context, ancestors),
        "bookmarkStart" => return Node::element(XHTML, "span").with_attr("id", n.attr("name")),
        "hyperlink" => {
            let mut href = context.links.get(n.attr("id")).cloned().unwrap_or_default();
            if href.is_empty() && !n.attr("anchor").is_empty() {
                href = format!("#{}", n.attr("anchor"));
            }
            out = out.with_attr("href", href);
        }
        "drawing" | "pict" => {
            drawing(n, context, ancestors, &mut out.children);
            return out;
        }
        "p" => {
            let props = n.child("pPr");
            match heading(props, &context.styles) {
                0 if !props.child("numPr").name.local.is_empty() => out.name.local = "li".to_owned(),
                0 => {}
                level => {
                    out.name.local = format!("h{}", level.min(6));
                    out = out.with_attr(DEPTH_ATTR, (level - 1).to_string());
                }
            }
        }
        _ => {}
    }
    for c in &n.children {
        // XML indentation is not document text.
        if c.name.local.is_empty() && local != "t" {
            continue;
        }
        out.children.push(convert(c, context, ancestors));
    }
    if local == "r" {
        let props = n.child("rPr");
        for style in ["i", "b"] {
            if is_on(props.child(style)) {
                out = Node { children: vec![out], ..Node::element(XHTML, style) };
            }
        }
    }
    out
}

/// The heading level of a paragraph, 0 for body text: the outline level
/// of the paragraph or its inherited styles, else a style named "Title" or
/// "Heading n".
fn heading<'a>(props: &'a Node, styles: &HashMap<&'a str, &'a Node>) -> usize {
    let (mut props, mut id) = (props, props.child("pStyle").attr("val"));
    let mut fallback = 0;
    for _ in 0..MAX_DEPTH {
        let outline = props.child("outlineLvl");
        if !outline.name.local.is_empty() {
            // Level 9 is body text.
            return outline.attr("val").parse::<usize>().ok().filter(|&level| level < 9).map_or(0, |level| level + 1);
        }
        let style = styles.get(id).copied();
        let name = style.map(|s| s.child("name").attr("val")).filter(|name| !name.is_empty()).unwrap_or(id);
        let name = name.to_lowercase().replace(' ', "");
        if fallback == 0 {
            if name == "title" {
                fallback = 1;
            }
            if let Some(&[digit @ b'1'..=b'9']) = name.strip_prefix("heading").map(str::as_bytes) {
                fallback = usize::from(digit - b'0');
            }
        }
        let Some(style) = style else { break };
        (props, id) = (style.child("pPr"), style.child("basedOn").attr("val"));
    }
    fallback
}

/// Whether a run property such as `w:b` or `w:i` is switched on.
fn is_on(n: &Node) -> bool {
    !matches!(n.attr("val"), "0" | "false" | "off") && !n.name.local.is_empty()
}

/// The first choice whose required namespaces the reader understands, else
/// the fallback.
fn alternate<'a>(n: &'a Node, ancestors: &[&'a Node]) -> &'a Node {
    let supported = |choice: &Node| {
        let requires: Vec<&str> = choice.attr("Requires").split_whitespace().collect();
        !requires.is_empty() && requires.iter().all(|prefix| namespace(choice, ancestors, prefix).is_some_and(|ns| KNOWN_NAMESPACES.contains(&ns)))
    };
    n.children.iter().find(|c| c.name.local == "Choice" && supported(c)).unwrap_or_else(|| n.child("Fallback"))
}

/// The namespace `prefix` stands for at `n`.
fn namespace<'a>(n: &'a Node, ancestors: &[&'a Node], prefix: &str) -> Option<&'a str> {
    std::iter::once(n)
        .chain(ancestors.iter().rev().copied())
        .find_map(|node| node.attrs.iter().find(|a| a.name.space == "xmlns" && a.name.local == prefix).map(|a| a.value.as_str()))
}

/// Collects the pictures and text box paragraphs of a drawing's chosen
/// branches, in document order; `n` is the last of `ancestors`.
fn drawing<'a>(n: &'a Node, context: &Context<'_>, ancestors: &mut Vec<&'a Node>, out: &mut Vec<Node>) {
    let image = |id: &str| context.images.get(id).map(|name| image_node(name));
    match n.name.local.as_str() {
        "AlternateContent" => {
            let branch = alternate(n, ancestors);
            ancestors.push(branch);
            drawing(branch, context, ancestors, out);
            ancestors.pop();
            return;
        }
        "blip" => return out.extend(image(n.attr("embed"))),
        "imagedata" => return out.extend(image(n.attr("id"))),
        // Both DrawingML and VML (v:textbox) text boxes hold their paragraphs here.
        "txbxContent" => return out.push(convert_element(n, context, ancestors)),
        _ => {}
    }
    for c in &n.children {
        ancestors.push(c);
        drawing(c, context, ancestors, out);
        ancestors.pop();
    }
}
