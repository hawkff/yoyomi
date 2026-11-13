use std::borrow::Cow;
use std::collections::HashSet;

use yoyomi_core::{Chapter, Error, Result};

/// The bookmarks of a ComicInfo.xml file, sorted by page. Bookmarks without
/// a title or outside the book's `pages` drop out.
pub(super) fn comic_info(xml: &[u8], pages: usize) -> Result<Vec<Chapter>> {
    let invalid = |error: &dyn std::fmt::Display| Error::Format(format!("ComicInfo.xml: {error}"));
    let text = std::str::from_utf8(xml).map_err(|e| invalid(&e))?;
    let text = without_doctype(text.trim_start_matches('\u{feff}')).ok_or_else(|| invalid(&"unterminated DOCTYPE"))?;
    let document = roxmltree::Document::parse(&text).map_err(|e| invalid(&e))?;
    let mut chapters = Vec::new();
    let bookmarks =
        document.root_element().children().filter(|n| n.has_tag_name("Pages")).flat_map(|n| n.children()).filter(|n| n.has_tag_name("Page"));
    for page in bookmarks {
        let image: i64 = page.attribute("Image").unwrap_or("0").trim().parse().map_err(|e| invalid(&e))?;
        let title = page.attribute("Bookmark").unwrap_or_default();
        match usize::try_from(image) {
            Ok(page) if page < pages && !title.trim().is_empty() => chapters.push(Chapter { title: title.into(), page, depth: 0 }),
            _ => {}
        }
    }
    chapters.sort_by_key(|chapter| chapter.page);
    Ok(chapters)
}

/// `text` without its DOCTYPE declaration, internal subset included, or
/// `None` when the declaration does not end. Nothing it declares is ever
/// expanded, so a reference to a declared entity fails to parse; DTD parsing
/// would expand entities from an untrusted archive.
fn without_doctype(text: &str) -> Option<Cow<'_, str>> {
    const DOCTYPE: &str = "<!DOCTYPE";
    let Some(start) = text.find(DOCTYPE) else { return Some(Cow::Borrowed(text)) };
    let rest = &text[start + DOCTYPE.len()..];
    let bytes = rest.as_bytes();
    let (mut quote, mut subset, mut i) = (None, false, 0);
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (Some(open), byte) if byte == open => quote = None,
            (Some(_), _) => {}
            (None, byte @ (b'"' | b'\'')) => quote = Some(byte),
            (None, b'[') => subset = true,
            (None, b']') => subset = false,
            // A comment in the subset may hold quotes and brackets.
            (None, b'<') if subset && rest[i..].starts_with("<!--") => i += rest[i..].find("-->")? + 2,
            (None, b'>') if !subset => return Some(Cow::Owned(format!("{}{}", &text[..start], &rest[i + 1..]))),
            _ => {}
        }
        i += 1;
    }
    None
}

/// One chapter per run of pages in the same folder, named by the folder's
/// path, or "(root)" for the top level. Pages in a single folder make no
/// chapters.
pub(super) fn folders<'a>(names: impl Iterator<Item = &'a str>) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    let mut seen = HashSet::new();
    let mut current = None;
    for (page, name) in names.enumerate() {
        // The innermost folder names the chapter, so a single wrapping folder still splits.
        let folder = name.rsplit_once('/').map_or("", |(folder, _)| folder);
        seen.insert(folder);
        if current != Some(folder) {
            current = Some(folder);
            let title = if folder.is_empty() { "(root)" } else { folder };
            chapters.push(Chapter { title: title.into(), page, depth: 0 });
        }
    }
    if seen.len() < 2 { Vec::new() } else { chapters }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(title: &str, page: usize) -> Chapter {
        Chapter { title: title.into(), page, depth: 0 }
    }

    #[test]
    fn comic_info_bookmarks_sort_by_page_and_skip_invalid_pages() {
        let xml = br#"<ComicInfo><Pages><Page Image="1" Bookmark="Second"/><Page Image="0" Bookmark="First"/><Page Image="1" Bookmark="Same page"/><Page Image="99" Bookmark="Invalid"/><Page Image="0" Bookmark=" "/></Pages></ComicInfo>"#;
        assert_eq!(comic_info(xml, 2).unwrap(), [chapter("First", 0), chapter("Second", 1), chapter("Same page", 1)]);
        assert!(comic_info(b"<ComicInfo>", 2).is_err());
    }

    #[test]
    fn comic_info_with_a_doctype_keeps_its_bookmarks() {
        let body = r#"<ComicInfo><Pages><Page Image="1" Bookmark="Second"/></Pages></ComicInfo>"#;
        let declared = format!("<?xml version=\"1.0\"?>\n<!DOCTYPE ComicInfo SYSTEM \"ComicInfo.dtd\">\n{body}");
        assert_eq!(comic_info(declared.as_bytes(), 2).unwrap(), comic_info(body.as_bytes(), 2).unwrap());
        assert_eq!(comic_info(body.as_bytes(), 2).unwrap(), [chapter("Second", 1)]);
    }

    #[test]
    fn declared_entities_are_never_expanded() {
        // Expanded, the bookmark would take 512 KiB x 4000 = 2 GiB.
        let entity = "A".repeat(512 << 10);
        let bookmark = "&big;".repeat(4000);
        let xml = format!(
            "<!DOCTYPE ComicInfo [<!ENTITY big \"{entity}\">]><ComicInfo><Pages><Page Image=\"0\" Bookmark=\"{bookmark}\"/></Pages></ComicInfo>"
        );
        assert!(xml.len() < 1 << 20);
        let error = comic_info(xml.as_bytes(), 1).unwrap_err();
        assert!(error.to_string().contains("big"), "{error}");
    }

    #[test]
    fn a_doctype_is_skipped_whole() {
        let body = r#"<ComicInfo><Pages><Page Image="0" Bookmark="First"/></Pages></ComicInfo>"#;
        let declared = format!("<!DOCTYPE ComicInfo SYSTEM \"a>b.dtd\" [ <!ENTITY x '1>2]'> <!-- don't ] > --> <!ELEMENT ComicInfo ANY> ]>\n{body}");
        assert_eq!(comic_info(declared.as_bytes(), 1).unwrap(), [chapter("First", 0)]);
        for unterminated in ["<!DOCTYPE ComicInfo [ <ComicInfo/>", "<!DOCTYPE ComicInfo \"<ComicInfo/>", "<!DOCTYPE ComicInfo [<!-- ]><ComicInfo/>"] {
            assert!(comic_info(unterminated.as_bytes(), 1).unwrap_err().to_string().contains("unterminated DOCTYPE"), "{unterminated}");
        }
    }

    #[test]
    fn folders_name_runs_of_pages() {
        assert_eq!(folders(["First/1.png", "Last/2.png"].into_iter()), [chapter("First", 0), chapter("Last", 1)]);
        assert_eq!(folders(["Vol 1/Ch 1/1.png", "Vol 1/Ch 2/2.png"].into_iter()), [chapter("Vol 1/Ch 1", 0), chapter("Vol 1/Ch 2", 1)]);
        assert_eq!(folders(["cover.png", "ch1/1.png"].into_iter()), [chapter("(root)", 0), chapter("ch1", 1)]);
        assert!(folders(["ch1/1.png", "ch1/2.png"].into_iter()).is_empty());
    }
}
