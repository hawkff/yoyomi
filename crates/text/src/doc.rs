//! Word 97-2003 documents through `antiword -x db`, whose DocBook output
//! keeps the headings.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use yoyomi_core::{Error, MAX_PAGE_BYTES, Result};

use crate::book::TextBook;
use crate::layout::{DEPTH_ATTR, MAX_DOCUMENT_TOKENS, Package, file_name, invalid, layout_body, read_bounded};
use crate::xml::{self, Node, XHTML};

const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DIAGNOSTICS: u64 = 4 << 10;

pub(crate) fn open(path: &Path) -> Result<TextBook> {
    open_with("antiword", path)
}

fn open_with(tool: &str, path: &Path) -> Result<TextBook> {
    let file = file_name(path);
    let out = run(tool, path).map_err(|e| invalid(format!("{file}: {e}")))?;
    let doc = xml::parse(&out, MAX_DOCUMENT_TOKENS).map_err(|e| invalid(format!("{file}: {e}")))?;
    let body = Node { children: vec![convert(&doc, 0)], ..Node::element(XHTML, "body") };
    let layout = layout_body(&file, &mut Package::default(), body, &Default::default())?;
    let chapters = Ok(layout.chapters.clone());
    Ok(TextBook::new(path, String::new(), layout, None, chapters))
}

/// Runs `tool -x db path` and returns its output, within the page size
/// limit and the two-minute tool timeout.
fn run(tool: &str, path: &Path) -> Result<Vec<u8>> {
    let failed = |message: String| Error::Tool { tool: tool.to_owned(), message };
    let mut child = Command::new(tool)
        .args(["-x", "db"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| if e.kind() == io::ErrorKind::NotFound { Error::Unsupported(format!("{tool} is not on PATH")) } else { Error::Io(e) })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = thread::spawn(move || read_bounded(stdout, MAX_PAGE_BYTES));
    // Keep a prefix of the diagnostics and drain the rest, so stderr cannot block the tool.
    let diagnostics = thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = stderr.by_ref().take(MAX_DIAGNOSTICS).read_to_end(&mut kept);
        let _ = io::copy(&mut stderr, &mut io::sink());
        kept
    });
    let deadline = Instant::now() + TOOL_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let out = out.join().expect("the output reader does not panic");
    let diagnostics = diagnostics.join().expect("the diagnostics reader does not panic");
    let Some(status) = status else { return Err(failed(format!("gave no result within {}s", TOOL_TIMEOUT.as_secs()))) };
    if !status.success() {
        let message = String::from_utf8_lossy(&diagnostics).trim().chars().take(200).collect::<String>();
        return Err(failed(if message.is_empty() { status.to_string() } else { message }));
    }
    out.map_err(|e| failed(e.to_string()))
}

/// Converts one DocBook element to XHTML; sections nest the heading level.
fn convert(n: &Node, level: usize) -> Node {
    let local = n.name.local.as_str();
    if local.is_empty() {
        return Node::text(n.text.clone());
    }
    let mut out = Node::element(XHTML, "span");
    if !n.attr("id").is_empty() {
        out = out.with_attr("id", n.attr("id"));
    }
    let mut level = level;
    let name = match local {
        "bookinfo" | "articleinfo" | "info" => return Node::default(),
        "book" | "article" => "div".to_owned(),
        "chapter" | "preface" | "appendix" | "section" | "sect1" | "sect2" | "sect3" | "sect4" | "sect5" => {
            level += 1;
            "div".to_owned()
        }
        "title" => {
            out = out.with_attr(DEPTH_ATTR, level.saturating_sub(1).to_string());
            format!("h{}", level.clamp(1, 6))
        }
        "para" | "simpara" => "p".to_owned(),
        "beginpage" => "br".to_owned(),
        "literallayout" | "programlisting" => "pre".to_owned(),
        "emphasis" => match n.attr("role") {
            "" | "italic" => "em",
            "bold" => "strong",
            _ => "span",
        }
        .to_owned(),
        "itemizedlist" => "ul".to_owned(),
        "orderedlist" => "ol".to_owned(),
        "listitem" => "li".to_owned(),
        "row" => "tr".to_owned(),
        "entry" => "td".to_owned(),
        "ulink" => {
            out = out.with_attr("href", n.attr("url"));
            "a".to_owned()
        }
        "link" => {
            out = out.with_attr("href", format!("#{}", n.attr("linkend")));
            "a".to_owned()
        }
        _ => "span".to_owned(),
    };
    out.name.local = name;
    for c in &n.children {
        // The book title repeats the metadata, not a chapter.
        if matches!(local, "book" | "article") && c.name.local == "title" {
            continue;
        }
        out.children.push(convert(c, level));
    }
    out
}

#[cfg(test)]
mod tests {
    use yoyomi_core::{Book, Cancel, Chapter};

    use super::*;

    const PARAGRAPHS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/paragraphs.doc");

    #[test]
    fn a_missing_tool_is_not_on_path() {
        let missing = open_with("yoyomi-no-such-tool", Path::new(PARAGRAPHS)).err().expect("a missing tool fails").to_string();
        assert!(missing.contains("yoyomi-no-such-tool is not on PATH"), "{missing}");
    }

    #[test]
    fn antiword_reads_doc_paragraphs() {
        if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir| dir.join("antiword").is_file()) {
            eprintln!("skipped: antiword is not installed");
            return;
        }
        let path = Path::new(PARAGRAPHS);
        let book = open(path).unwrap();
        assert_eq!((book.title(), book.len()), ("paragraphs", 1));
        assert!(book.can_invert(0));
        assert_eq!(book.page_text(0).unwrap(), ["Hello paragraph one.", "Second paragraph here."]);
    }

    #[test]
    fn emphasis_roles_pick_the_style() {
        for (role, want) in [("", "em"), ("italic", "em"), ("bold", "strong"), ("underline", "span"), ("strikethrough", "span")] {
            let doc = xml::parse(format!(r#"<emphasis role="{role}">Text</emphasis>"#).as_bytes(), 100).unwrap();
            let got = convert(&doc, 0);
            assert_eq!((got.name.local.as_str(), got.all_text().as_str()), (want, "Text"), "{role:?}");
        }
    }

    #[test]
    fn sections_nest_chapters() {
        let doc = xml::parse(
            b"<book><title>Metadata title</title><bookinfo><title>Metadata title</title></bookinfo><chapter><title>First</title><para>Text<beginpage/>After break</para><sect1><title>Nested</title><para>More text</para></sect1></chapter></book>",
            1000,
        )
        .unwrap();
        let body = Node { children: vec![convert(&doc, 0)], ..Node::element(XHTML, "body") };
        let layout = layout_body("book.doc", &mut Package::default(), body, &Default::default()).unwrap();
        let chapters = layout.chapters.clone();
        let book = TextBook::new(Path::new("/books/book.doc"), String::new(), layout, None, Ok(chapters));
        assert_eq!(book.page_text(0).unwrap(), ["First", "Text", "After break"]);
        let want = [Chapter { title: "First".into(), page: 0, depth: 0 }, Chapter { title: "Nested".into(), page: 1, depth: 1 }];
        assert_eq!(book.chapters(&Cancel::new()).unwrap(), want);
    }
}
