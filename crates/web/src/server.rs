//! The HTTP side of the browser reader: one-time links, the token, the book
//! index, the reader page and the page bytes.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use yoyomi_core::{Book, Cancel, Error, Result};

use crate::owner;

/// The server binds a random port in `FIRST_PORT..FIRST_PORT + PORT_COUNT`.
const FIRST_PORT: u16 = 50000;
const PORT_COUNT: u16 = 10000;
const PORT_TRIES: usize = 50;

/// The most bytes a request line and its headers may take. Cookies are
/// shared by every port on 127.0.0.1, so a browser may send many.
const MAX_HEAD_BYTES: usize = 1 << 20;

/// How long a connection may take to read the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connection may take to send its whole request head.
const HEAD_DEADLINE: Duration = Duration::from_secs(5);

/// The most requests served at once; the accept loop closes connections past
/// it, so slow clients cannot hold an unbounded number of threads.
const MAX_REQUESTS: usize = 16;

/// How long the server waits after a failed accept, as when it runs out of
/// file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// Serves the open books at `http://127.0.0.1:<port>/<token>/b/<n>/`. It
/// starts on the first `open` and stops on `close` or drop.
#[derive(Default)]
pub struct Server {
    state: Arc<Mutex<State>>,
    running: Option<Running>,
}

struct Running {
    port: u16,
    stop: Arc<AtomicBool>,
    accepting: JoinHandle<()>,
}

#[derive(Default)]
struct State {
    /// The served books in pane order, `None` for an empty pane.
    books: Vec<Option<Arc<dyn Book>>>,
    port: u16,
    /// The random path prefix that keeps other local users out.
    token: String,
    /// The one-time code of the latest link from `open`, until its use.
    code: Option<String>,
    /// Where that link leads, below the token.
    next: String,
}

impl Server {
    /// Replaces the served books; the index is the pane order.
    pub fn set_books(&self, books: Vec<Option<Arc<dyn Book>>>) {
        self.state.lock().books = books;
    }

    /// Starts the server unless it runs and returns a link to `path`, given
    /// relative to the reader root, such as `b/0/?p=3`. The link carries a
    /// one-time code instead of the token: a browser keeps its launch URL on
    /// its command line, where other local users can read it, and a spent
    /// code opens nothing. A newer link replaces an unused older one.
    pub fn open(&mut self, path: &str) -> Result<String> {
        let port = self.start()?;
        let code = random_text()?;
        let mut state = self.state.lock();
        state.next = path.to_owned();
        state.code = Some(code.clone());
        Ok(format!("http://127.0.0.1:{port}/open/{code}"))
    }

    /// The bound port while the server runs.
    pub fn port(&self) -> Option<u16> {
        self.running.as_ref().map(|running| running.port)
    }

    /// Stops accepting connections and frees the port.
    pub fn close(&mut self) {
        let Some(running) = self.running.take() else { return };
        running.stop.store(true, Ordering::SeqCst);
        // The accept loop checks the flag after each connection.
        let wake = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, running.port)), IO_TIMEOUT);
        if wake.is_ok() {
            let _ = running.accepting.join();
        }
    }

    /// Binds a random free port and accepts connections on a thread, unless
    /// the server runs already. Returns the port.
    fn start(&mut self) -> Result<u16> {
        if let Some(running) = &self.running {
            return Ok(running.port);
        }
        let seeds: [u8; 2 * PORT_TRIES] = random()?;
        let mut failure = io::Error::other("no port tried");
        let mut bound = None;
        for seed in seeds.as_chunks::<2>().0 {
            let port = FIRST_PORT + u16::from_ne_bytes(*seed) % PORT_COUNT;
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => {
                    bound = Some((listener, port));
                    break;
                }
                Err(error) => failure = error,
            }
        }
        let Some((listener, port)) = bound else {
            let last = FIRST_PORT + PORT_COUNT - 1;
            return Err(Error::Io(io::Error::new(failure.kind(), format!("no free port in {FIRST_PORT}-{last}: {failure}"))));
        };
        let token = random_text()?;
        {
            let mut state = self.state.lock();
            state.port = port;
            state.token = token;
            state.code = None;
        }
        let (state, stop) = (self.state.clone(), Arc::new(AtomicBool::new(false)));
        let accepting = {
            let stop = stop.clone();
            thread::Builder::new().name("web".into()).spawn(move || accept(&listener, &state, &stop))?
        };
        self.running = Some(Running { port, stop, accepting });
        Ok(port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close();
    }
}

/// Opens `url` in the default browser: `open` on macOS, `xdg-open` elsewhere.
pub fn open_in_browser(url: &str) -> io::Result<()> {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut child = Command::new(program).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    // Reaps the launcher; a failed spawn leaves it a zombie until yy exits.
    let _ = thread::Builder::new().name("browser".into()).spawn(move || child.wait());
    Ok(())
}

/// Serves each connection of this user on its own thread until `stop`, at
/// most `MAX_REQUESTS` at once.
fn accept(listener: &TcpListener, state: &Arc<Mutex<State>>, stop: &AtomicBool) {
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = owner::accept_owned(listener, owner::peer_uid);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match accepted {
            Ok(stream) => {
                let slot = Slot(active.clone());
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_REQUESTS {
                    continue;
                }
                let state = state.clone();
                // A failed spawn drops the connection and the slot.
                let _ = thread::Builder::new().name("web request".into()).spawn(move || {
                    let _slot = slot;
                    serve(stream, &state);
                });
            }
            Err(_) => thread::sleep(ACCEPT_RETRY),
        }
    }
}

/// One counted request; dropping it frees its place, also on a panic.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answers one request and closes the connection.
fn serve(mut stream: TcpStream, state: &Mutex<State>) {
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let response = match read_head(&mut stream, Instant::now() + HEAD_DEADLINE) {
        Ok(head) => respond(state, &head),
        Err(_) => Response::text(400, "bad request"),
    };
    let _ = response.write_to(&mut stream);
}

/// Reads the request line and headers, up to the empty line, until
/// `deadline`.
fn read_head(stream: &mut TcpStream, deadline: Instant) -> io::Result<String> {
    let mut head = Vec::new();
    let mut chunk = [0; 1024];
    // Where the search for the empty line resumes, so a long head is scanned once.
    let mut searched = 0;
    loop {
        if let Some(end) = head[searched..].windows(4).position(|window| window == b"\r\n\r\n") {
            head.truncate(searched + end);
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
        searched = head.len().saturating_sub(3);
        if head.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too long"));
        }
        // A zero timeout would mean none, so a passed deadline ends here.
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(left))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.extend_from_slice(&chunk[..n]);
    }
}

/// Answers a GET request from its request line and headers.
fn respond(state: &Mutex<State>, head: &str) -> Response {
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (request.next(), request.next()) else { return Response::text(400, "bad request") };
    if !target.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Response::text(400, "bad request");
    }
    if method != "GET" {
        return Response::text(405, "only GET");
    }
    let host = lines.find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("host")).map(|(_, value)| value.trim()));
    route(state, host.unwrap_or_default(), target)
}

/// Routes a request for `target` to `host`. Only 127.0.0.1 with the bound
/// port passes, which stops DNS rebinding; one-time codes redeem without the
/// token, and everything else lives below it.
fn route(state: &Mutex<State>, host: &str, target: &str) -> Response {
    let (authority, token) = {
        let state = state.lock();
        (format!("127.0.0.1:{}", state.port), state.token.clone())
    };
    if host != authority {
        return Response::text(403, "invalid host");
    }
    if let Some(code) = target.strip_prefix("/open/") {
        return redeem(state, code);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path.strip_prefix('/').and_then(|path| path.strip_prefix(token.as_str())) {
        _ if token.is_empty() => Response::not_found(),
        Some("/") => {
            let books = state.lock().books.clone();
            index(&books)
        }
        Some(path) if path.starts_with("/b/") => book(state, path, query),
        _ => Response::not_found(),
    }
}

/// Trades the one-time code for a redirect below the token, which stays out
/// of every command line.
fn redeem(state: &Mutex<State>, code: &str) -> Response {
    let mut state = state.lock();
    if !state.code.as_deref().is_some_and(|expected| same(code.as_bytes(), expected.as_bytes())) {
        return Response::text(410, "This link was used already. Press e in yy for a new one.");
    }
    state.code = None;
    Response::redirect(303, format!("/{}/{}", state.token, state.next))
}

/// Compares two strings in a time that depends only on their lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Routes `/b/<n>/` (the reader page of pane `n`) and `/b/<n>/page/<p>`
/// (page bytes of the book in pane `n` now).
fn book(state: &Mutex<State>, path: &str, query: &str) -> Response {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let n = parts.get(1).and_then(|n| n.parse::<usize>().ok());
    let book = n.and_then(|n| state.lock().books.get(n).cloned().flatten());
    let Some(book) = book else { return Response::not_found() };
    match parts[..] {
        [_, _, "page", page] => page_bytes(book.as_ref(), page),
        // The reader's links are relative to the slash, and the redirect is
        // relative too, so it keeps the token.
        [_, n] if !path.ends_with('/') => Response::redirect(301, format!("{n}/?{query}")),
        [_, _] => {
            let page = query_value(query, "p").and_then(|p| p.parse().ok()).filter(|&p| p < book.len()).unwrap_or(0);
            let invertible: Vec<bool> = (0..book.len()).map(|page| book.can_invert(page)).collect();
            Response::html(reader_page(book.title(), page, query_value(query, "invert") == Some("1"), &invertible))
        }
        _ => Response::not_found(),
    }
}

fn page_bytes(book: &dyn Book, page: &str) -> Response {
    let Some(page) = page.parse::<usize>().ok().filter(|&page| page < book.len()) else { return Response::not_found() };
    match book.encoded(page, &Cancel::new()) {
        Ok(encoded) => Response {
            status: 200,
            // The URL names a pane, which may serve another book next.
            headers: vec![("Content-Type", encoded.mime.into()), ("Cache-Control", "no-store".into())],
            body: encoded.bytes,
        },
        Err(_) => Response::not_found(),
    }
}

/// The first value of `key` in a query string.
fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
}

const INDEX: &str = r#"<!doctype html>
<meta charset="utf-8"><title>yoyomi</title>
<style>
body{background:#111;color:#ddd;font:16px/1.6 system-ui;max-width:40rem;margin:3rem auto;padding:0 1rem}
a{color:#8cf;text-decoration:none} a:hover{text-decoration:underline}
</style>
<h1>yoyomi</h1>
{{BOOKS}}"#;

/// The token root: a link to each open book.
fn index(books: &[Option<Arc<dyn Book>>]) -> Response {
    let mut items = String::new();
    for (n, book) in books.iter().enumerate() {
        if let Some(book) = book {
            let _ = write!(items, "<li><a href=\"b/{n}/\">{}</a> — {} pages</li>", escape(book.title()), book.len());
        }
    }
    let empty = if items.is_empty() { "<p>No books open.</p>" } else { "" };
    Response::html(INDEX.replace("{{BOOKS}}", &format!("{empty}\n<ul>{items}</ul>")))
}

const READER: &str = r#"<!doctype html>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{TITLE}}</title>
<style>
body{background:#111;color:#ddd;font:14px system-ui;margin:0;display:flex;flex-direction:column;height:100vh}
header{padding:.4rem .8rem;display:flex;gap:1rem;align-items:center;background:#1a1a1a}
header a{color:#8cf;text-decoration:none}
main{flex:1;display:flex;align-items:center;justify-content:center;overflow:hidden}
img{max-width:100%;max-height:100%;object-fit:contain}
img.paper{background:#fff}
img.inverted{filter:invert(1)}
</style>
<header>
  <a href="../../">index</a><strong>{{TITLE}}</strong>
  <span id="pos"></span>
  <span style="opacity:.6">h/l or ←/→ pages, g/G first/last, i invert</span>
</header>
<main><img id="pg" alt="page"></main>
<script>
const pages={{PAGES}}, invertible=[{{INVERTIBLE}}];let p={{PAGE}}, inverted={{INVERTED}};
const img=document.getElementById('pg'),pos=document.getElementById('pos');
function show(){p=Math.max(0,Math.min(pages-1,p));const src='page/'+p;
if(img.getAttribute('src')!==src){img.style.visibility='hidden';img.src=src;}
img.classList.toggle('inverted',inverted&&invertible[p]);img.classList.toggle('paper',!invertible[p]);
pos.textContent=(p+1)+'/'+pages;history.replaceState(null,'','?p='+p+(inverted?'&invert=1':''));
if(p+1<pages){(new Image()).src='page/'+(p+1);}}
addEventListener('keydown',e=>{
if(e.key==='l'||e.key==='ArrowRight'||e.key===' ')p++;
else if(e.key==='h'||e.key==='ArrowLeft')p--;
else if(e.key==='g')p=0;else if(e.key==='G')p=pages-1;
else if(e.key==='i')inverted=!inverted;else return;
e.preventDefault();show();});
img.addEventListener('click',e=>{p+=(e.clientX>innerWidth/2)?1:-1;show();});
img.addEventListener('load',()=>{if(img.complete)img.style.visibility='visible';});
img.addEventListener('error',()=>{if(img.complete)img.style.visibility='visible';});
show();
</script>"#;

/// The reader page of a book: one page at a time, with the keys in its
/// header, starting at `page`. `invertible` says which pages inversion
/// applies to.
fn reader_page(title: &str, page: usize, inverted: bool, invertible: &[bool]) -> String {
    let invertible: Vec<&str> = invertible.iter().map(|&on| if on { "true" } else { "false" }).collect();
    // The title goes in last, so its text cannot hold a placeholder.
    READER
        .replace("{{PAGES}}", &invertible.len().to_string())
        .replace("{{INVERTIBLE}}", &invertible.join(","))
        .replace("{{PAGE}}", &page.to_string())
        .replace("{{INVERTED}}", if inverted { "true" } else { "false" })
        .replace("{{TITLE}}", &escape(title))
}

/// Escapes text for HTML.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Arc<[u8]>,
}

impl Response {
    fn text(status: u16, text: &str) -> Response {
        let headers = vec![("Content-Type", "text/plain; charset=utf-8".into()), ("X-Content-Type-Options", "nosniff".into())];
        Response { status, headers, body: format!("{text}\n").into_bytes().into() }
    }

    fn not_found() -> Response {
        Response::text(404, "404 page not found")
    }

    fn html(page: String) -> Response {
        Response { status: 200, headers: vec![("Content-Type", "text/html; charset=utf-8".into())], body: page.into_bytes().into() }
    }

    fn redirect(status: u16, location: String) -> Response {
        Response { status, headers: vec![("Location", location)], body: Arc::new([]) }
    }

    fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            301 => "Moved Permanently",
            303 => "See Other",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            410 => "Gone",
            _ => "",
        };
        let mut head = format!("HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n", self.status, self.body.len());
        for (name, value) in &self.headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        head.push_str("\r\n");
        out.write_all(head.as_bytes())?;
        out.write_all(&self.body)?;
        out.flush()
    }
}

/// `N` bytes from the system's random source.
fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// 128 random bits in hex.
fn random_text() -> io::Result<String> {
    let bytes: [u8; 16] = random()?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Child;
    use std::time::Instant;

    use yoyomi_core::{Encoded, PageKind, Pixels};

    use super::*;

    /// A book whose pages are stored PNG bytes, or `None` for a page that
    /// fails to encode, as an oversized one does.
    struct FakeBook {
        title: &'static str,
        pages: Vec<Option<Vec<u8>>>,
    }

    fn fake_book(title: &'static str, pages: Vec<Option<Vec<u8>>>) -> Arc<dyn Book> {
        Arc::new(FakeBook { title, pages })
    }

    impl Book for FakeBook {
        fn path(&self) -> &Path {
            Path::new("/books/comic.cbz")
        }
        fn title(&self) -> &str {
            self.title
        }
        fn len(&self) -> usize {
            self.pages.len()
        }
        fn kind(&self, _page: usize) -> PageKind {
            PageKind::Image
        }
        fn page(&self, _page: usize, _cancel: &Cancel) -> Result<Pixels> {
            Err(Error::unsupported("no pixels"))
        }
        fn encoded(&self, page: usize, _cancel: &Cancel) -> Result<Encoded> {
            match &self.pages[page] {
                Some(bytes) => Ok(Encoded { bytes: bytes.clone().into(), mime: "image/png" }),
                None => Err(Error::Limit("page exceeds 32000000 decoded pixels".into())),
            }
        }
    }

    /// The state of a server on `port` 54321 behind the token `secret`.
    fn serving(books: Vec<Arc<dyn Book>>) -> Mutex<State> {
        Mutex::new(State { books: books.into_iter().map(Some).collect(), port: 54321, token: "secret".into(), ..State::default() })
    }

    fn get(state: &Mutex<State>, target: &str) -> Response {
        route(state, "127.0.0.1:54321", target)
    }

    impl Response {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(key, _)| *key == name).map(|(_, value)| value.as_str())
        }
    }

    fn body(response: &Response) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    /// Fetches `url` over HTTP and returns the status and the Location header.
    fn fetch(url: &str) -> (u16, String) {
        let rest = url.strip_prefix("http://").unwrap();
        let (authority, path) = rest.split_at(rest.find('/').unwrap());
        let mut stream = TcpStream::connect(authority).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n").unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        let status = answer.split(' ').nth(1).unwrap().parse().unwrap();
        let location = answer.lines().find_map(|line| line.strip_prefix("Location: ")).unwrap_or_default();
        (status, location.to_owned())
    }

    #[test]
    fn authorize_rejects_foreign_host_and_token() {
        let state = serving(vec![fake_book("comic", vec![None])]);
        for (host, target, want) in [
            ("attacker.example:54321", "/secret/b/0/", 403),
            ("127.0.0.1:54322", "/secret/b/0/", 403),
            ("127.0.0.1:54321", "/b/0/", 404),
            ("127.0.0.1:54321", "/guess/b/0/", 404),
            ("127.0.0.1:54321", "/secretly/b/0/", 404),
            ("127.0.0.1:54321", "/secret/b/0/", 200),
        ] {
            assert_eq!(route(&state, host, target).status, want, "{host}{target}");
        }
        let head = "GET /secret/b/0/ HTTP/1.1\r\nUser-Agent: test\r\nhost: 127.0.0.1:54321";
        assert_eq!(respond(&state, head).status, 200, "header names ignore case");
        assert_eq!(respond(&state, &head.replace("GET", "POST")).status, 405);
        assert_eq!(respond(&state, "GET /secret/b/0/\x7f HTTP/1.1\r\nHost: 127.0.0.1:54321").status, 400);
    }

    #[test]
    fn open_links_work_once() {
        let mut server = Server::default();
        server.set_books(vec![Some(fake_book("comic", vec![None]))]);
        let stale = server.open("b/0/?p=0").unwrap();
        let link = server.open("b/0/?p=0&invert=1").unwrap();
        let token = server.state.lock().token.clone();
        assert!(!link.contains(&token) && token.len() >= 20, "link {link:?} leaks the token");
        let port = server.port().unwrap();
        assert!((50000..60000).contains(&port));
        assert_eq!(fetch(&stale).0, 410, "a replaced link");
        let (status, target) = fetch(&link);
        assert_eq!((status, target.as_str()), (303, format!("/{token}/b/0/?p=0&invert=1").as_str()), "first use");
        assert_eq!(fetch(&link).0, 410, "second use");
        let root = format!("http://127.0.0.1:{port}");
        assert_eq!(fetch(&format!("{root}{target}")).0, 200);
        assert_eq!(fetch(&format!("{root}/")).0, 404);
        assert_eq!(server.open("b/0/").unwrap().split_once("/open/").unwrap().0, root, "a new link keeps the port");
        server.close();
        assert_eq!(server.port(), None);
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "the port stays bound");
    }

    #[test]
    fn slow_request_heads_end_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        // Each byte arrives well within a read timeout, the head never ends.
        let trickle = thread::spawn(move || {
            while client.write_all(b"G").is_ok() {
                thread::sleep(Duration::from_millis(20));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let start = Instant::now();
        let error = read_head(&mut stream, start + Duration::from_millis(300)).unwrap_err();
        assert!(matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2), "read for {:?}", start.elapsed());
        drop(stream);
        trickle.join().unwrap();
    }

    #[test]
    fn connections_past_the_cap_close() {
        let mut server = Server::default();
        server.open("b/0/").unwrap();
        let addr = (Ipv4Addr::LOCALHOST, server.port().unwrap());
        // Connections that send nothing hold their places until the head deadline.
        let mut held: Vec<TcpStream> = (0..MAX_REQUESTS).map(|_| TcpStream::connect(addr).unwrap()).collect();
        let mut refused = TcpStream::connect(addr).unwrap();
        refused.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let closed = match refused.read(&mut [0]) {
            Ok(n) => n == 0,
            Err(error) => error.kind() == io::ErrorKind::ConnectionReset,
        };
        assert!(closed, "a connection past the cap stayed open");
        held.pop();
        let served = (0..100).any(|_| {
            thread::sleep(Duration::from_millis(20));
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = write!(stream, "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.1);
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            answer.starts_with("HTTP/1.1 404")
        });
        assert!(served, "a freed place serves no request");
    }

    #[test]
    fn the_token_root_lists_open_books() {
        let state = serving(Vec::new());
        assert!(body(&get(&state, "/secret/")).contains("No books open."));
        state.lock().books = vec![None, Some(fake_book("<Tom & Jerry>", vec![None; 3]))];
        let index = get(&state, "/secret/");
        assert_eq!(index.header("Content-Type"), Some("text/html; charset=utf-8"));
        assert!(body(&index).contains(r#"<li><a href="b/1/">&lt;Tom &amp; Jerry&gt;</a> — 3 pages</li>"#), "{}", body(&index));
        assert!(!body(&index).contains("b/0/"), "an empty pane has no link");
        assert_eq!(get(&state, "/secret/b/0/").status, 404);
    }

    #[test]
    fn page_urls_do_not_cache_replaced_books() {
        let server = Server::default();
        {
            let mut state = server.state.lock();
            state.port = 54321;
            state.token = "secret".into();
        }
        for content in [b"first", b"other"] {
            server.set_books(vec![Some(fake_book("comic", vec![Some(content.to_vec())]))]);
            let page = get(&server.state, "/secret/b/0/page/0");
            assert_eq!((page.status, page.header("Cache-Control"), &*page.body), (200, Some("no-store"), content.as_slice()));
            assert_eq!(page.header("Content-Type"), Some("image/png"));
        }
        for target in ["/secret/b/0/page/1", "/secret/b/0/page/x", "/secret/b/1/page/0", "/secret/b/0/other/0", "/secret/b/0/1/page/0"] {
            assert_eq!(get(&server.state, target).status, 404, "{target}");
        }
    }

    #[test]
    fn an_open_reader_page_loads_the_pane_s_new_book() {
        let state = serving(vec![fake_book("first", vec![Some(b"first".to_vec()); 3])]);
        let reader = body(&get(&state, "/secret/b/0/?p=2"));
        assert!(reader.contains("const src='page/'+p;"), "the reader page links page URLs relative to its pane");
        state.lock().books = vec![Some(fake_book("second", vec![Some(b"second".to_vec())]))];
        let page = get(&state, "/secret/b/0/page/0");
        assert_eq!((page.status, &*page.body), (200, b"second".as_slice()), "the open reader's next request");
        assert_eq!(get(&state, "/secret/b/0/page/2").status, 404, "a page past the new book's end");
    }

    #[test]
    fn request_heads_up_to_a_mebibyte_pass() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        for (cookie, accepted) in [(MAX_HEAD_BYTES - 100, true), (MAX_HEAD_BYTES + 2048, false)] {
            let head = format!("GET / HTTP/1.1\r\nHost: {addr}\r\nCookie: {}\r\n\r\n", "c".repeat(cookie));
            // A thread sends, since the head outgrows the socket buffers.
            let client = thread::spawn(move || {
                let mut client = TcpStream::connect(addr).unwrap();
                let _ = client.write_all(head.as_bytes());
                client
            });
            let (mut stream, _) = listener.accept().unwrap();
            let read = read_head(&mut stream, Instant::now() + Duration::from_secs(10));
            assert_eq!(read.is_ok(), accepted, "a {cookie}-byte cookie");
            if let Ok(head) = read {
                assert!(head.ends_with('c') && head.len() > cookie, "the head lost bytes");
            }
            drop(stream);
            drop(client.join().unwrap());
        }
    }

    #[test]
    fn reader_redirect_keeps_relative_links() {
        let state = serving(vec![fake_book("comic", vec![None])]);
        // Relative to /secret/b/0, "0/?p=1" keeps the token.
        let redirect = get(&state, "/secret/b/0?p=1");
        assert_eq!((redirect.status, redirect.header("Location")), (301, Some("0/?p=1")));
    }

    #[test]
    fn page_rejects_oversized_dimensions() {
        let state = serving(vec![fake_book("comic", vec![None])]);
        assert_eq!(get(&state, "/secret/b/0/page/0").status, 404);
    }

    #[test]
    fn reader_inversion_query() {
        let state = serving(vec![fake_book("comic", vec![None])]);
        for query in ["", "0", "1", "true", "<script>"] {
            let page = get(&state, &format!("/secret/b/0/?invert={query}"));
            let want = if query == "1" { "inverted=true;" } else { "inverted=false;" };
            let text: String = body(&page).split_whitespace().collect();
            assert!(page.status == 200 && text.contains(want) && text.contains("invertible=[true]"), "inversion query {query:?}");
        }
        state.lock().books = vec![Some(fake_book("comic", vec![None; 3]))];
        for (query, want) in [("p=2", "letp=2,"), ("p=3", "letp=0,"), ("p=-1", "letp=0,")] {
            let text: String = body(&get(&state, &format!("/secret/b/0/?{query}"))).split_whitespace().collect();
            assert!(text.contains(want), "{query}");
        }
    }

    #[test]
    fn reader_page_lays_only_pages_that_keep_their_colors_on_white() {
        let page = reader_page("Book", 0, false, &[false]);
        let style = page.split_once("<style>").unwrap().1.split_once("</style>").unwrap().0;
        assert!(style.contains("img.paper{background:#fff}") && style.contains("img.inverted{filter:invert(1)}"), "{style}");
        assert_eq!(style.matches("background:#fff").count(), 1, "{style}");
        assert!(page.contains("img.classList.toggle('paper',!invertible[p]);"), "the script lost the paper toggle");
    }

    #[test]
    fn reader_template_carries_per_page_inversion() {
        let page = reader_page("Mixed EPUB", 0, true, &[true, false, true]);
        assert!(page.split_whitespace().collect::<String>().contains("invertible=[true,false,true]"), "reader lost per-page inversion eligibility");
        let path = std::env::var_os("PATH").unwrap_or_default();
        if !std::env::split_paths(&path).any(|dir| dir.join("node").is_file()) {
            eprintln!("skipped: the browser interaction check needs Node.js");
            return;
        }
        let script = page.split_once("<script>").unwrap().1.split_once("</script>").unwrap().0;
        let check = r#"
const assert=require('node:assert/strict'), vm=require('node:vm'), fs=require('node:fs');
const events={}, preloads=[];let src='', filtered=false, paper=false, requests=0, url='';
const img={complete:false,style:{},classList:{toggle(name,on){if(name==='inverted')filtered=on;else if(name==='paper')paper=on;else throw new Error(name);}},
getAttribute(){return src;},get src(){return src;},set src(value){src=value;this.complete=false;requests++;},
addEventListener(name,fn){events[name]=fn;}};
vm.runInNewContext(fs.readFileSync(0,'utf8'),{
document:{getElementById(id){return id==='pg'?img:{};}},Image:function(){preloads.push(this);},innerWidth:100,
history:{replaceState(state,title,value){url=value;}},addEventListener(name,fn){events[name]=fn;}});
const key=k=>events.keydown({key:k,preventDefault(){}});
const load=()=>{img.complete=true;events.load();};
assert.equal(filtered,true);assert.equal(paper,false);assert.equal(img.style.visibility,'hidden');assert.equal(preloads.at(-1).src,'page/1');load();
key('i');assert.equal(filtered,false);assert.equal(paper,false);assert.equal(requests,1);assert.equal(img.style.visibility,'visible');
key('i');assert.equal(filtered,true);
key('l');assert.equal(src,'page/1');assert.equal(filtered,false);assert.equal(paper,true);assert.equal(img.style.visibility,'hidden');
load();assert.equal(img.style.visibility,'visible');assert.equal(filtered,false);
key('l');assert.equal(filtered,true);assert.equal(paper,false);assert.equal(img.style.visibility,'hidden');
key('i');assert.equal(filtered,false);assert.equal(paper,false);assert.equal(requests,3);load();
key('i');assert.equal(filtered,true);assert.equal(requests,3);assert.equal(url,'?p=2&invert=1');
events.click({clientX:0});assert.equal(filtered,false);assert.equal(paper,true);load();
key('i');assert.equal(filtered,false);assert.equal(paper,true);
"#;
        let mut node = Command::new("node").args(["-e", check]).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        node.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let status = wait(&mut node, Duration::from_secs(30));
        let mut errors = String::new();
        node.stderr.take().unwrap().read_to_string(&mut errors).unwrap();
        assert!(status.is_some_and(|status| status.success()), "browser inversion: {errors}");
    }

    /// Waits for `child` until `limit`, then kills it.
    fn wait(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        None
    }
}
