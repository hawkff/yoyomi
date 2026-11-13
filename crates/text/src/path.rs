//! References between the files of a book: resource paths that must stay
//! inside the archive, and link targets.

/// Resolves `reference`, relative to the document `base`, to a file of the
/// archive. Web addresses, queries, absolute paths and paths that climb out
/// of the archive fail.
pub(crate) fn resolve(base: &str, reference: &str) -> Result<String, String> {
    let local = || format!("EPUB requires a local resource reference: {reference:?}");
    let (path, fragment) = reference.split_once('#').unwrap_or((reference, ""));
    if reference.bytes().any(|b| b < 0x20 || b == 0x7f) || path.contains('?') || scheme(path) != Ok(None) || path.starts_with("//") {
        return Err(local());
    }
    unescape(fragment).ok_or_else(local)?;
    let path = unescape(path).ok_or_else(local)?;
    if path.starts_with('/') || path.contains(['\\', '\0', ':']) {
        return Err(format!("EPUB unsafe resource path: {reference:?}"));
    }
    let resolved = if path.is_empty() { base.to_owned() } else { clean(&format!("{}/{path}", dir(base))) };
    if resolved.is_empty() || resolved == "." || resolved == ".." || resolved.starts_with("../") {
        return Err(format!("EPUB resource escapes archive: {reference:?}"));
    }
    Ok(resolved)
}

/// Where a link in document `name` leads: a web or mail address (external),
/// or the anchor key of a place in the book. Other schemes and references
/// that leave the archive lead nowhere.
pub(crate) fn link_target(name: &str, href: &str) -> Option<(String, bool)> {
    let href = href.trim();
    let (doc, fragment) = href.split_once('#').unwrap_or((href, ""));
    if href.is_empty() || href.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let fragment = unescape(fragment)?;
    unescape(doc)?;
    if let Some(scheme) = scheme(doc).ok()? {
        let web = ["http", "https", "mailto"].iter().any(|s| s.eq_ignore_ascii_case(scheme));
        return web.then(|| (href.to_owned(), false));
    }
    let mut target = if doc.is_empty() { name.to_owned() } else { resolve(name, doc).ok()? };
    if !fragment.is_empty() {
        target = format!("{target}#{fragment}");
    }
    Some((target, true))
}

/// The scheme of a URL reference: the text before its first colon when that
/// text starts with a letter and holds only letters, digits, `+`, `-` and
/// `.`. Other references have no scheme, and a leading colon is an error.
fn scheme(reference: &str) -> Result<Option<&str>, ()> {
    for (i, c) in reference.char_indices() {
        match c {
            'a'..='z' | 'A'..='Z' => {}
            '0'..='9' | '+' | '-' | '.' if i > 0 => {}
            ':' if i == 0 => return Err(()),
            ':' => return Ok(Some(&reference[..i])),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Decodes percent escapes; a broken escape or invalid UTF-8 gives `None`.
fn unescape(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The folder of a slash-separated path: what comes before its last slash,
/// cleaned, so `.` when nothing does.
fn dir(path: &str) -> String {
    clean(path.rsplit_once('/').map_or("", |(dir, _)| dir))
}

/// The shortest equivalent slash-separated path. Empty and `.` parts drop
/// out, and a `..` removes the part before it unless that part is `..` too.
/// A `..` with nothing to remove stays in a relative path and drops out of a
/// rooted one. A rooted path keeps its leading slash, and a relative path
/// with nothing left is `.`.
pub(crate) fn clean(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if rooted => {}
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_resolve_inside_the_archive() {
        for (reference, want) in
            [("../images/a%20b.png#part", "OPS/images/a b.png"), ("#here", "OPS/text/ch.xhtml"), ("../../cover.png", "cover.png")]
        {
            assert_eq!(resolve("OPS/text/ch.xhtml", reference).as_deref(), Ok(want), "{reference}");
        }
        for reference in [
            "../../../escape",
            "%2e%2e/%2e%2e/%2e%2e/escape",
            "https://example.org/a",
            "//example.org/a",
            "/a",
            "%2fa",
            "a%5cb",
            "file:a",
            "a?x=1",
            "a?",
            "bad%zz",
            "a%00b",
            "C:/a",
        ] {
            assert!(resolve("OPS/text/ch.xhtml", reference).is_err(), "accepted {reference:?}");
        }
        assert_eq!(resolve("", "image-0").as_deref(), Ok("image-0"));
    }

    #[test]
    fn links_lead_to_anchors_or_the_web() {
        for (href, want) in [
            ("a.xhtml#sec%3A2", Some(("OPS/text/a.xhtml#sec:2", true))),
            ("#n1", Some(("OPS/text/z.xhtml#n1", true))),
            (" HTTPS://example.com/?q=1 ", Some(("HTTPS://example.com/?q=1", false))),
            ("mailto:someone@example.com", Some(("mailto:someone@example.com", false))),
            ("file:///etc/passwd", None),
            ("javascript:alert(1)", None),
            ("../../../evil.xhtml", None),
            ("", None),
        ] {
            let got = link_target("OPS/text/z.xhtml", href);
            assert_eq!(got.as_ref().map(|(target, internal)| (target.as_str(), *internal)), want, "{href:?}");
        }
    }

    #[test]
    fn paths_clean_to_their_shortest_equivalent() {
        for (path, want) in [("a/../b", "b"), ("./a//b/", "a/b"), ("..", ".."), ("a/../..", ".."), ("", "."), ("/../a", "/a")] {
            assert_eq!(clean(path), want, "{path}");
        }
    }
}
