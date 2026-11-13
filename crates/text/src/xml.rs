//! XML documents as trees that keep text and elements in source order, with
//! limits: a token budget, a depth limit, no internal DTD and no `xml:base`.

use std::borrow::Cow;

use encoding_rs::Encoding;
use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesRef, Event};
use quick_xml::name::ResolveResult;

pub(crate) const XHTML: &str = "http://www.w3.org/1999/xhtml";
pub(crate) const OPS: &str = "http://www.idpf.org/2007/ops";
pub(crate) const MATHML: &str = "http://www.w3.org/1998/Math/MathML";
pub(crate) const SVG: &str = "http://www.w3.org/2000/svg";
const XML: &str = "http://www.w3.org/XML/1998/namespace";

/// How deep elements may nest.
pub(crate) const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Name {
    /// The namespace, or the unbound prefix, or empty.
    pub space: String,
    pub local: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Attr {
    /// Namespace declarations keep `xmlns` as their space and the prefix as
    /// their local name.
    pub name: Name,
    pub value: String,
}

/// An element, or a text node when its local name is empty.
#[derive(Clone, Debug, Default)]
pub(crate) struct Node {
    pub name: Name,
    pub attrs: Vec<Attr>,
    pub text: String,
    pub children: Vec<Node>,
}

static EMPTY: Node = Node { name: Name { space: String::new(), local: String::new() }, attrs: Vec::new(), text: String::new(), children: Vec::new() };

impl Node {
    pub fn element(space: &str, local: &str) -> Node {
        Node { name: Name { space: space.to_owned(), local: local.to_owned() }, ..Node::default() }
    }

    pub fn text(text: impl Into<String>) -> Node {
        Node { text: text.into(), ..Node::default() }
    }

    pub fn is(&self, space: &str, local: &str) -> bool {
        self.name.space == space && self.name.local == local
    }

    /// The value of the first attribute with this local name, or "".
    pub fn attr(&self, local: &str) -> &str {
        self.attrs.iter().find(|a| a.name.local == local).map_or("", |a| a.value.as_str())
    }

    pub fn with_attr(mut self, local: &str, value: impl Into<String>) -> Node {
        self.attrs.push(Attr { name: Name { space: String::new(), local: local.to_owned() }, value: value.into() });
        self
    }

    /// The first child element with this local name, or an empty node.
    pub fn child(&self, local: &str) -> &Node {
        self.children.iter().find(|c| c.name.local == local).unwrap_or(&EMPTY)
    }

    /// The text as a title reads: ruby readings and `epub:case` branches
    /// left out, MathML as its alternative text, whitespace collapsed.
    pub fn all_text(&self) -> String {
        fn walk(n: &Node, out: &mut String) {
            if n.name.space == XHTML && (n.name.local == "rt" || n.name.local == "rp") || n.is(OPS, "case") {
                return;
            }
            if n.is(MATHML, "math") && !n.attr("alttext").trim().is_empty() {
                out.push_str(n.attr("alttext"));
                return;
            }
            out.push_str(&n.text);
            for c in &n.children {
                walk(c, out);
            }
        }
        let mut out = String::new();
        walk(self, &mut out);
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Parses one XML document of at most `max_tokens` tokens. A byte order mark
/// picks UTF-8 or UTF-16; otherwise the declared charset decodes the bytes.
/// Named HTML entities resolve, as EPUB 2 XHTML uses them without a DTD.
pub(crate) fn parse(data: &[u8], max_tokens: usize) -> Result<Node, String> {
    let text = decode(data)?;
    let mut reader = NsReader::from_str(&text);
    reader.config_mut().expand_empty_elements = true;
    let xml_error = |e: quick_xml::Error| format!("XML: {e}");
    let mut stack = vec![Node::default()];
    let (mut tokens, mut in_text) = (0, false);
    loop {
        let (space, event) = reader.read_resolved_event().map_err(xml_error)?;
        let space = owned(space);
        let text = matches!(event, Event::Text(_) | Event::GeneralRef(_) | Event::CData(_));
        if !matches!(event, Event::Eof) && !(text && in_text) {
            tokens += 1;
            if tokens > max_tokens {
                return Err(format!("XML exceeds {max_tokens} tokens"));
            }
        }
        in_text = text;
        match event {
            Event::Eof => break,
            Event::Start(start) => {
                if stack.len() > MAX_DEPTH {
                    return Err(format!("XML exceeds depth {MAX_DEPTH}"));
                }
                let mut node = Node { name: Name { space, local: start.local_name().as_ref().to_owned() }, ..Node::default() };
                for attr in start.attributes() {
                    let attr = attr.map_err(|e| format!("XML: {e}"))?;
                    let key = attr.key.0;
                    let name = match key.split_once(':') {
                        None => Name { space: String::new(), local: key.to_owned() },
                        Some(("xmlns", prefix)) => Name { space: "xmlns".to_owned(), local: prefix.to_owned() },
                        Some(("xml", local)) => Name { space: XML.to_owned(), local: local.to_owned() },
                        Some((_, local)) => Name { space: owned(reader.resolver().resolve_attribute(attr.key).0), local: local.to_owned() },
                    };
                    if name.space == XML && name.local == "base" {
                        return Err("xml:base is unsupported".to_owned());
                    }
                    let value = attr.normalized_value_with(XmlVersion::Implicit1_0, 1, resolve_predefined_entity).map_err(xml_error)?;
                    node.attrs.push(Attr { name, value: value.into_owned() });
                }
                stack.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().expect("an end tag follows its start tag");
                stack.last_mut().expect("the root sentinel stays").children.push(node);
            }
            Event::Text(t) => push_text(&mut stack, &t.xml10_content())?,
            Event::CData(t) => push_text(&mut stack, &t.xml10_content())?,
            Event::GeneralRef(r) => push_text(&mut stack, &entity(&r)?)?,
            Event::DocType(t) if t.xml10_content().contains('[') => return Err("XML internal DTD is unsupported".to_owned()),
            _ => {}
        }
    }
    let mut root = stack.pop().expect("the root sentinel stays");
    if !stack.is_empty() || root.children.len() != 1 {
        return Err("XML needs one complete root".to_owned());
    }
    Ok(root.children.pop().expect("one root"))
}

fn owned(space: ResolveResult<'_>) -> String {
    match space {
        ResolveResult::Bound(ns) => ns.as_ref().to_owned(),
        ResolveResult::Unbound => String::new(),
        ResolveResult::Unknown(prefix) => prefix,
    }
}

fn entity(r: &BytesRef<'_>) -> Result<String, String> {
    if let Some(c) = r.resolve_char_ref().map_err(|e| format!("XML: {e}"))? {
        return Ok(c.to_string());
    }
    resolve_predefined_entity(r).map(str::to_owned).ok_or_else(|| format!("XML: unknown entity &{};", &**r))
}

/// Appends text to the open element, joining it with the text before.
fn push_text(stack: &mut [Node], text: &str) -> Result<(), String> {
    if stack.len() == 1 {
        if !text.trim().is_empty() {
            return Err("XML text outside root".to_owned());
        }
        return Ok(());
    }
    let parent = stack.last_mut().expect("an open element");
    match parent.children.last_mut() {
        Some(last) if last.name.local.is_empty() => last.text.push_str(text),
        _ => parent.children.push(Node::text(text)),
    }
    Ok(())
}

/// The document as UTF-8: a byte order mark wins, then the charset in the
/// XML declaration; FB2 files often declare windows-1251 or koi8-r.
fn decode(data: &[u8]) -> Result<Cow<'_, str>, String> {
    if let Some((encoding, bom)) = Encoding::for_bom(data) {
        return Ok(encoding.decode_without_bom_handling(&data[bom..]).0);
    }
    match declared_charset(data) {
        Some(label) if !label.eq_ignore_ascii_case("utf-8") && !label.to_ascii_lowercase().starts_with("utf-16") => {
            let encoding = Encoding::for_label(label.as_bytes()).ok_or_else(|| format!("XML: unsupported charset {label:?}"))?;
            Ok(encoding.decode_without_bom_handling(data).0)
        }
        _ => std::str::from_utf8(data).map(Cow::Borrowed).map_err(|_| "XML: invalid UTF-8".to_owned()),
    }
}

/// The `encoding` of a leading XML declaration.
fn declared_charset(data: &[u8]) -> Option<&str> {
    let head = data.strip_prefix(b"<?xml")?;
    let end = head.windows(2).position(|w| w == b"?>")?;
    let decl = std::str::from_utf8(&head[..end]).ok()?;
    let rest = decl[decl.find("encoding")? + "encoding".len()..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let rest = &rest[1..];
    Some(&rest[..rest.find(quote)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_keep_text_and_elements_in_order() {
        let doc = parse(b"<a xmlns=\"urn:x\" xmlns:p=\"urn:p\">one<b p:k=\"v\">two</b>&amp;&nbsp;<![CDATA[<c>]]></a>", 100).unwrap();
        assert!(doc.is("urn:x", "a"));
        assert_eq!(doc.children.len(), 3, "{doc:?}");
        assert_eq!(doc.children[0].text, "one");
        let b = doc.child("b");
        assert_eq!((b.attr("k"), b.attrs[0].name.space.as_str()), ("v", "urn:p"));
        assert_eq!(doc.children[2].text, "&\u{a0}<c>");
        assert_eq!(doc.attrs[1].name, Name { space: "xmlns".into(), local: "p".into() });
    }

    #[test]
    fn bounds_and_unsupported_markup_fail() {
        let deep = format!("<r>{}{}</r>", "<n>".repeat(MAX_DEPTH), "</n>".repeat(MAX_DEPTH));
        assert!(parse(deep.as_bytes(), 1_000_000).unwrap_err().contains("depth"));
        let wide = format!("<r>{}</r>", "<n/>".repeat(100));
        assert!(parse(wide.as_bytes(), 202).is_ok());
        assert!(parse(wide.as_bytes(), 201).unwrap_err().contains("tokens"));
        assert!(parse(br#"<!DOCTYPE r [<!ENTITY x "value">]><r>&x;</r>"#, 100).unwrap_err().contains("DTD"));
        assert!(parse(br#"<r xml:base="../"/>"#, 100).unwrap_err().contains("xml:base"));
        assert!(parse(b"<r/><s/>", 100).unwrap_err().contains("one complete root"));
        assert!(parse(b"<r>", 100).is_err());
        assert!(parse(b"x<r/>", 100).unwrap_err().contains("outside root"));
    }

    #[test]
    fn charsets_come_from_the_mark_or_the_declaration() {
        let (cyrillic, _, _) = encoding_rs::WINDOWS_1251.encode("<?xml version=\"1.0\" encoding=\"windows-1251\"?><r>Привет</r>");
        assert_eq!(parse(&cyrillic, 100).unwrap().children[0].text, "Привет");
        let (koi8, _, _) = encoding_rs::KOI8_R.encode("<?xml version='1.0' encoding='koi8-r'?><r>Привет</r>");
        assert_eq!(parse(&koi8, 100).unwrap().children[0].text, "Привет");
        let mut wide = vec![0xff, 0xfe];
        wide.extend("<?xml version=\"1.0\" encoding=\"UTF-16\"?><r>hi</r>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(parse(&wide, 100).unwrap().children[0].text, "hi");
    }

    #[test]
    fn titles_read_like_the_page() {
        let doc = parse(
            br#"<h xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">A <ruby>B<rt>b</rt></ruby> <epub:switch><epub:case>no</epub:case><epub:default>C</epub:default></epub:switch> <math xmlns="http://www.w3.org/1998/Math/MathML" alttext="x^2"><mi>x</mi></math></h>"#,
            100,
        )
        .unwrap();
        assert_eq!(doc.all_text(), "A B C x^2");
    }
}
