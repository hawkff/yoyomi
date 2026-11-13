//! PDF outlines from pdftohtml and DJVU outlines from djvused.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use yoyomi_core::{Cancel, Chapter, Error, Result};

use super::Tool;
use super::tool::run;

/// The most bytes djvused may print for an outline.
const MAX_OUTLINE_BYTES: usize = 1 << 20;

/// The most bytes pdftohtml may print for one page and the outline, and
/// djvused for its page listing.
const MAX_LISTING_BYTES: usize = 4 << 20;

/// How deep outline entries may nest.
const MAX_OUTLINE_DEPTH: usize = 128;

/// How many entries a DJVU outline may hold.
const MAX_OUTLINE_ENTRIES: usize = 10_000;

/// Reads the outline of the document at `path`, which has `pages` pages.
pub(super) fn chapters(path: &Path, tool: Tool, pages: usize, stops: &[&Cancel]) -> Result<Vec<Chapter>> {
    match tool {
        Tool::Pdf => {
            // Poppler prints the whole outline even when it converts only one page.
            let mut command = Command::new("pdftohtml");
            command.args(["-xml", "-i", "-q", "-enc", "UTF-8", "-f", "1", "-l", "1", "-stdout"]).arg(path);
            pdf_outline(&run(&mut command, MAX_LISTING_BYTES, stops)?, pages)
        }
        Tool::Djvu => {
            let outline = run(Command::new("djvused").args(["-u", "-e", "print-outline"]).arg(path), MAX_OUTLINE_BYTES, stops)?;
            if outline.trim_ascii().is_empty() {
                return Ok(Vec::new());
            }
            let listing = run(Command::new("djvused").args(["-u", "-e", "ls"]).arg(path), MAX_LISTING_BYTES, stops)?;
            djvu_outline(&outline, pages, &djvu_page_names(&String::from_utf8_lossy(&listing)))
        }
    }
}

/// Reads the `outline` element of pdftohtml's XML. Entries without a title or
/// with a page outside the book drop out; an entry without a page leads to
/// its first child.
fn pdf_outline(data: &[u8], pages: usize) -> Result<Vec<Chapter>> {
    let text = std::str::from_utf8(data).map_err(|e| Error::Format(format!("pdftohtml output: {e}")))?;
    let options = roxmltree::ParsingOptions { allow_dtd: true, ..roxmltree::ParsingOptions::default() };
    let doc = roxmltree::Document::parse_with_options(text, options).map_err(|e| Error::Format(format!("pdftohtml output: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "pdf2xml" {
        return Err(Error::Format("pdftohtml returned no PDF metadata".into()));
    }
    match root.children().find(|n| n.has_tag_name("outline")) {
        Some(outline) => pdf_entries(outline, 0, pages),
        None => Ok(Vec::new()),
    }
}

fn pdf_entries(outline: roxmltree::Node, depth: usize, pages: usize) -> Result<Vec<Chapter>> {
    if depth >= MAX_OUTLINE_DEPTH {
        return Err(Error::Format(format!("PDF outline nests deeper than {MAX_OUTLINE_DEPTH} levels")));
    }
    let mut entries: Vec<(Option<usize>, String, usize)> = Vec::new();
    let mut parent = None;
    for node in outline.children().filter(roxmltree::Node::is_element) {
        match node.tag_name().name() {
            "item" => {
                parent = None;
                let title = collapse_spaces(&node.descendants().filter(roxmltree::Node::is_text).filter_map(|n| n.text()).collect::<String>());
                let target = node.attribute("page").unwrap_or("");
                let page = target.parse::<usize>().ok().filter(|page| (1..=pages).contains(page));
                if !title.is_empty() && (target.is_empty() || page.is_some()) {
                    parent = Some(entries.len());
                    entries.push((page.map(|page| page - 1), title, depth));
                }
            }
            "outline" => {
                let children = pdf_entries(node, depth + 1, pages)?;
                if let Some(parent) = parent
                    && entries[parent].0.is_none()
                    && let Some(first) = children.first()
                {
                    entries[parent].0 = Some(first.page);
                }
                entries.extend(children.into_iter().map(|c| (Some(c.page), c.title, c.depth)));
            }
            _ => {}
        }
    }
    Ok(entries.into_iter().filter_map(|(page, title, depth)| Some(Chapter { title, page: page?, depth })).collect())
}

/// Maps the page ids, saved names and titles in djvused's `ls` listing to
/// zero-based pages. Lines read `<page> P <size>  <id>[ F=<name>][ T=<title>]`.
fn djvu_page_names(listing: &str) -> HashMap<String, usize> {
    let mut names = HashMap::new();
    for line in listing.lines() {
        let Some((page, name)) = djvu_page_line(line) else { continue };
        let (id, title) = name.split_once(" T=").unwrap_or((name, ""));
        let (id, saved) = id.split_once(" F=").unwrap_or((id, ""));
        for name in [id, saved, title] {
            if !name.is_empty() {
                names.insert(name.to_owned(), page);
            }
        }
    }
    names
}

/// The zero-based page and the name part of one page line of `ls`.
fn djvu_page_line(line: &str) -> Option<(usize, &str)> {
    let (number, rest) = split_digits(line.trim_start())?;
    let page = number.parse::<usize>().ok()?.checked_sub(1)?;
    let rest = rest.strip_prefix(" P")?;
    let size = rest.trim_start();
    if size.len() == rest.len() {
        return None;
    }
    let (_, rest) = split_digits(size)?;
    Some((page, rest.strip_prefix("  ")?))
}

/// Splits a non-empty run of ASCII digits off the start of `text`.
fn split_digits(text: &str) -> Option<(&str, &str)> {
    let end = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// Reads djvused's `print-outline`: `(bookmarks (title destination
/// children...)...)`. Entries without a title or with a destination outside
/// the book drop out; an entry without a destination leads to its first
/// child.
fn djvu_outline(data: &[u8], pages: usize, names: &HashMap<String, usize>) -> Result<Vec<Chapter>> {
    let mut scanner = Scanner { data: data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data), at: 0, entries: 0 };
    if scanner.token()? != Token::Open || scanner.token()? != Token::Ident("bookmarks") {
        return Err(Error::Format("DJVU outline lacks bookmarks".into()));
    }
    let chapters = scanner.djvu_entries(0, pages, names)?;
    if scanner.token()? != Token::End {
        return Err(Error::Format("DJVU outline has trailing data".into()));
    }
    Ok(chapters)
}

/// The zero-based page a DJVU outline destination names: `#<page number>`,
/// `#<page id>` or `?page=<page number or id>`. Relative pages (`#+1`) and
/// web addresses lead nowhere.
fn djvu_destination(target: &str, names: &HashMap<String, usize>) -> Option<usize> {
    let name = if let Some(name) = target.strip_prefix('#') { unescape(name, false)? } else { query_page(target.strip_prefix('?')?)? };
    if name.is_empty() || name.starts_with(['+', '-']) {
        return None;
    }
    match name.parse::<usize>() {
        Ok(page) => page.checked_sub(1),
        Err(_) => names.get(&name).copied(),
    }
}

/// The first `page` value of a URL query. A malformed pair spoils the whole
/// query.
fn query_page(query: &str) -> Option<String> {
    let mut page = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        if pair.contains(';') {
            return None;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (unescape(key, true)?, unescape(value, true)?);
        if key == "page" && page.is_none() {
            page = Some(value);
        }
    }
    page
}

/// Decodes `%XX` escapes and, in a query, `+` as a space.
fn unescape(text: &str, query: bool) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        match byte {
            b'%' => {
                let hex = std::str::from_utf8(rest.get(..2)?).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok().filter(|_| hex.bytes().all(|b| b.is_ascii_hexdigit()))?);
                rest = &rest[2..];
            }
            b'+' if query => bytes.push(b' '),
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

/// Joins the words of `text` with single spaces.
fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Open,
    Close,
    Ident(&'a str),
    /// A quoted string with its escapes decoded.
    Text(Vec<u8>),
    /// Any other character.
    Other,
    End,
}

/// Splits a djvused outline into parentheses, identifiers and C-style quoted
/// strings.
struct Scanner<'a> {
    data: &'a [u8],
    at: usize,
    entries: usize,
}

impl<'a> Scanner<'a> {
    fn token(&mut self) -> Result<Token<'a>> {
        while self.data.get(self.at).is_some_and(|b| b" \t\r\n".contains(b)) {
            self.at += 1;
        }
        let Some(&byte) = self.data.get(self.at) else { return Ok(Token::End) };
        self.at += 1;
        Ok(match byte {
            b'(' => Token::Open,
            b')' => Token::Close,
            b'"' => Token::Text(self.quoted()?),
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = self.at - 1;
                while self.data.get(self.at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
                    self.at += 1;
                }
                Token::Ident(std::str::from_utf8(&self.data[start..self.at]).expect("ASCII identifier"))
            }
            _ => Token::Other,
        })
    }

    /// The rest of a quoted string, after its opening quote.
    fn quoted(&mut self) -> Result<Vec<u8>> {
        let malformed = |what: &str| Error::Format(format!("DJVU outline: {what}"));
        let mut text = Vec::new();
        loop {
            let byte = *self.data.get(self.at).ok_or_else(|| malformed("string not terminated"))?;
            self.at += 1;
            match byte {
                b'"' => return Ok(text),
                b'\n' => return Err(malformed("string not terminated")),
                b'\\' => {
                    let escape = *self.data.get(self.at).ok_or_else(|| malformed("string not terminated"))?;
                    self.at += 1;
                    match escape {
                        b'a' => text.push(7),
                        b'b' => text.push(8),
                        b'f' => text.push(12),
                        b'n' => text.push(b'\n'),
                        b'r' => text.push(b'\r'),
                        b't' => text.push(b'\t'),
                        b'v' => text.push(11),
                        b'\\' | b'"' => text.push(escape),
                        b'0'..=b'7' => {
                            let digits = self.data.get(self.at - 1..self.at + 2).ok_or_else(|| malformed("bad octal escape"))?;
                            let value = digits
                                .iter()
                                .try_fold(0u32, |n, d| (b'0'..=b'7').contains(d).then(|| n * 8 + u32::from(d - b'0')))
                                .and_then(|n| u8::try_from(n).ok())
                                .ok_or_else(|| malformed("bad octal escape"))?;
                            text.push(value);
                            self.at += 2;
                        }
                        b'x' | b'u' | b'U' => {
                            let len = match escape {
                                b'x' => 2,
                                b'u' => 4,
                                _ => 8,
                            };
                            let digits = self.data.get(self.at..self.at + len).ok_or_else(|| malformed("bad hex escape"))?;
                            let value = std::str::from_utf8(digits)
                                .ok()
                                .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
                                .and_then(|d| u32::from_str_radix(d, 16).ok())
                                .ok_or_else(|| malformed("bad hex escape"))?;
                            self.at += len;
                            if escape == b'x' {
                                text.push(value as u8);
                            } else {
                                let c = char::from_u32(value).ok_or_else(|| malformed("bad unicode escape"))?;
                                text.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                            }
                        }
                        _ => return Err(malformed("unknown escape sequence")),
                    }
                }
                _ => text.push(byte),
            }
        }
    }

    fn text(&mut self) -> Result<String> {
        match self.token()? {
            Token::Text(text) => Ok(String::from_utf8_lossy(&text).into_owned()),
            _ => Err(Error::Format("DJVU outline needs a quoted title and destination".into())),
        }
    }

    /// The entries up to the parenthesis that closes their parent.
    fn djvu_entries(&mut self, depth: usize, pages: usize, names: &HashMap<String, usize>) -> Result<Vec<Chapter>> {
        let mut chapters = Vec::new();
        loop {
            let token = self.token()?;
            if token == Token::Close {
                return Ok(chapters);
            }
            self.entries += 1;
            if token != Token::Open || depth >= MAX_OUTLINE_DEPTH || self.entries > MAX_OUTLINE_ENTRIES {
                return Err(Error::Format("DJVU outline is malformed or exceeds its size limit".into()));
            }
            let (title, target) = (self.text()?, self.text()?);
            let mut page = djvu_destination(&target, names);
            let children = self.djvu_entries(depth + 1, pages, names)?;
            if target.is_empty()
                && let Some(first) = children.first()
            {
                page = Some(first.page);
            }
            let title = collapse_spaces(&title);
            if let Some(page) = page.filter(|&page| page < pages)
                && !title.is_empty()
            {
                chapters.push(Chapter { title, page, depth });
            }
            chapters.extend(children);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(title: &str, page: usize, depth: usize) -> Chapter {
        Chapter { title: title.into(), page, depth }
    }

    #[test]
    fn pdf_outline_rejects_invalid_targets() {
        let data = br#"<pdf2xml><outline><item page="2">Last</item><outline><item page="1">First &amp; nested</item><item page="0">Zero</item></outline><item page="3">Past end</item><item page="oops">Bad</item><item>External</item><item page="1"> </item><item>Part</item><outline><item page="2">Inside part</item></outline></outline></pdf2xml>"#;
        let want = [chapter("Last", 1, 0), chapter("First & nested", 0, 1), chapter("Part", 1, 0), chapter("Inside part", 1, 1)];
        assert_eq!(pdf_outline(data, 2).unwrap(), want);
        for data in ["<pdf2xml>", "<html/>"] {
            assert!(pdf_outline(data.as_bytes(), 2).is_err(), "accepted {data}");
        }
    }

    #[test]
    fn pdf_outline_reads_pdftohtml_documents() {
        let data = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE pdf2xml SYSTEM \"pdf2xml.dtd\">\n\n<pdf2xml producer=\"poppler\">\n<page number=\"1\"/>\n<outline>\n<item page=\"1\">Intro\n  duction</item>\n</outline>\n</pdf2xml>\n";
        assert_eq!(pdf_outline(data.as_bytes(), 1).unwrap(), [chapter("Intro duction", 0, 0)]);
        assert_eq!(pdf_outline(b"<pdf2xml><page/></pdf2xml>", 1).unwrap(), []);
        let deep = format!("<pdf2xml>{}{}</pdf2xml>", "<outline>".repeat(MAX_OUTLINE_DEPTH + 1), "</outline>".repeat(MAX_OUTLINE_DEPTH + 1));
        assert!(pdf_outline(deep.as_bytes(), 1).is_err());
    }

    #[test]
    fn djvu_outline_entries() {
        let names = djvu_page_names("     I      100  shared.djvu\n   1 P      100  cover.djvu\n   2 P      120  scan 02.djvu F=saved.djvu T=ii\r\n");
        let data = r##"(bookmarks ("Part \"one\"" "#1" ("Caf\303\251" "#scan%2002.djvu")) ("Group" "" ("Appendix" "?page=ii")) ("File" "#saved.djvu") ("Web" "https://example.com/#1") ("Missing" "#absent") ("Zero" "#0") ("Past end" "#3") ("Relative" "#+1"))"##;
        let want = [chapter("Part \"one\"", 0, 0), chapter("Café", 1, 1), chapter("Group", 1, 0), chapter("Appendix", 1, 1), chapter("File", 1, 0)];
        assert_eq!(djvu_outline(data.as_bytes(), 2, &names).unwrap(), want);
        let deep = format!("(bookmarks {}{}", r##"("Nested" "#1" "##.repeat(MAX_OUTLINE_DEPTH + 1), ")".repeat(MAX_OUTLINE_DEPTH + 2));
        for data in
            [r##"(bookmarks ("Truncated" "#1")"##, r##"(bookmarks (oops "#1"))"##, r##"(bookmarks ("Bad\q" "#1"))"##, "(bookmarks) extra", &deep]
        {
            assert!(djvu_outline(data.as_bytes(), 2, &HashMap::new()).is_err(), "accepted {data}");
        }
    }

    #[test]
    fn djvu_destinations() {
        let names = HashMap::from([("p two".to_owned(), 1)]);
        assert_eq!(djvu_destination("#2", &names), Some(1));
        assert_eq!(djvu_destination("#p%20two", &names), Some(1));
        assert_eq!(djvu_destination("?zoom=1&page=p+two", &names), Some(1));
        assert_eq!(djvu_destination("?page=3&page=1", &names), Some(2));
        for target in ["#0", "#-1", "#bad%2", "?page=1;x", "?zoom=1", "#", "2"] {
            assert_eq!(djvu_destination(target, &names), None, "{target}");
        }
    }
}
