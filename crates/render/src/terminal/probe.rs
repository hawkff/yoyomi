//! The startup queries and the parsing of their replies.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use yoyomi_core::CellSize;

use super::Terminal;

/// Asks whether the terminal draws kitty graphics. One that does answers
/// `ESC _ G i=31;OK ESC \` ahead of its DA1 reply.
pub(super) const KITTY_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

/// XTWINOPS cell size (`CSI 16 t`) and DA1 (`CSI c`).
const CELL_AND_ATTRIBUTES: &[u8] = b"\x1b[16t\x1b[c";

/// XTSMGRAPHICS color registers, then DA1 to end the wait.
const COLOR_REGISTERS: &[u8] = b"\x1b[?1;1;0S\x1b[c";

/// XTSMGRAPHICS color registers, the background color (OSC 11), then DA1.
const COLOR_REGISTERS_AND_BACKGROUND: &[u8] = b"\x1b[?1;1;0S\x1b]11;?\x1b\\\x1b[c";

/// Asks for the cell size and device attributes, preceded by the kitty query
/// in a herdr pane, and for the sixel color registers when the attributes
/// list sixel or `sixel` is set, with the background color in iTerm2.
/// `tty` must already be in non-canonical mode.
pub(super) fn query<T: Read + Write>(tty: &mut T, sixel: bool, herdr: bool, iterm2: bool) -> Terminal {
    let reply = if herdr { ask(tty, &[KITTY_QUERY, CELL_AND_ATTRIBUTES].concat()) } else { ask(tty, CELL_AND_ATTRIBUTES) };
    let mut terminal = Terminal { cell: cell_reply(&reply), sixel: has_sixel(&reply), kitty: has_kitty(&reply), ..Terminal::default() };
    if terminal.sixel || sixel {
        let reply = ask(tty, if iterm2 { COLOR_REGISTERS_AND_BACKGROUND } else { COLOR_REGISTERS });
        terminal.colors = color_registers(&reply);
        terminal.background = background_color(&reply);
    }
    terminal
}

/// Writes `query` and collects replies until the DA1 reply arrives, a read
/// returns nothing (VTIME ends a read after 300 ms of silence) or two seconds
/// pass. A slow link can deliver a reply in many small reads.
fn ask<T: Read + Write>(tty: &mut T, query: &[u8]) -> Vec<u8> {
    if tty.write_all(query).and_then(|()| tty.flush()).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let mut chunk = [0; 256];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match tty.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if device_attributes(&buf).is_some() || buf.len() > 4096 {
            break;
        }
    }
    buf
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The parameters of the first DA1 reply, `ESC [ ? params c`, in `reply`.
fn device_attributes(reply: &[u8]) -> Option<&[u8]> {
    let mut from = 0;
    while let Some(i) = find(&reply[from..], b"\x1b[?") {
        let start = from + i + 3;
        let len = reply[start..].iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
        if reply.get(start + len) == Some(&b'c') {
            return Some(&reply[start..start + len]);
        }
        from = start;
    }
    None
}

/// Whether a reply accepts [`KITTY_QUERY`].
fn has_kitty(reply: &[u8]) -> bool {
    find(reply, b"\x1b_Gi=31;OK\x1b\\").is_some()
}

/// Whether a DA1 reply lists sixel graphics (4) after the terminal class.
fn has_sixel(reply: &[u8]) -> bool {
    device_attributes(reply).is_some_and(|params| params.split(|b| *b == b';').skip(1).any(|a| a == b"4"))
}

/// The color count of an XTSMGRAPHICS reply, `ESC [ ? 1 ; 0 ; count S`, or 0.
fn color_registers(reply: &[u8]) -> u16 {
    let mut from = 0;
    while let Some(i) = find(&reply[from..], b"\x1b[?1;0;") {
        let start = from + i + 7;
        let len = reply[start..].iter().take_while(|b| b.is_ascii_digit()).count();
        if len > 0 && reply.get(start + len) == Some(&b'S') {
            // Only overflow can fail on a run of digits.
            return std::str::from_utf8(&reply[start..start + len]).map_or(0, |n| n.parse().unwrap_or(u16::MAX));
        }
        from = start;
    }
    0
}

/// The color of an OSC 11 reply, `ESC ] 11 ; rgb:R/G/B` ended by `ESC \` or
/// BEL, with one to four hex digits a component, each scaled by its width.
fn background_color(reply: &[u8]) -> Option<[u8; 3]> {
    let start = find(reply, b"\x1b]11;rgb:")? + 9;
    let len = reply[start..].iter().position(|b| *b == 0x1b || *b == 0x07)?;
    if reply[start + len] == 0x1b && reply.get(start + len + 1) != Some(&b'\\') {
        return None;
    }
    let mut parts = std::str::from_utf8(&reply[start..start + len]).ok()?.split('/');
    let mut rgb = [0u8; 3];
    for c in &mut rgb {
        let part = parts.next()?;
        if !(1..=4).contains(&part.len()) || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let max = (1u32 << (4 * part.len())) - 1;
        *c = ((u32::from_str_radix(part, 16).ok()? * 255 + max / 2) / max) as u8;
    }
    parts.next().is_none().then_some(rgb)
}

/// The cell size of the last XTWINOPS reply, `ESC [ 6 ; height ; width t`.
fn cell_reply(reply: &[u8]) -> Option<CellSize> {
    let start = reply.windows(4).rposition(|w| w == b"\x1b[6;")? + 4;
    let end = start + reply[start..].iter().position(|b| *b == b't')?;
    let (h, w) = std::str::from_utf8(&reply[start..end]).ok()?.split_once(';')?;
    let (h, w) = (h.parse::<u32>().ok()?, w.parse::<u32>().ok()?);
    (h > 0 && w > 0).then(|| CellSize { w: w.into(), h: h.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers each read with its next chunk, as a slow link does, and with
    /// nothing once they run out, as a read that VTIME ends. Records writes.
    #[derive(Default)]
    struct Trickle {
        chunks: Vec<&'static [u8]>,
        written: Vec<u8>,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.chunks.is_empty() {
                return Ok(0);
            }
            let chunk = self.chunks.remove(0);
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    impl Write for Trickle {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn trickle(chunks: &[&'static [u8]]) -> Trickle {
        Trickle { chunks: chunks.to_vec(), ..Trickle::default() }
    }

    #[test]
    fn ask_waits_for_a_reply_in_pieces() {
        let da1: &'static [u8] = b"\x1b[?62;4;22c";
        let mut tty = trickle(&[b"\x1b[6;17;8t"]);
        tty.chunks.extend(da1.chunks(1));
        let reply = ask(&mut tty, CELL_AND_ATTRIBUTES);
        // Bytes left unread would reach the reader as keys.
        assert!(has_sixel(&reply) && tty.chunks.is_empty(), "reply {reply:?} with {} chunks unread", tty.chunks.len());
    }

    #[test]
    fn ask_stops_at_the_reply_to_device_attributes() {
        let mut tty = trickle(&[b"\x1b[?62;22c", b"j"]);
        assert_eq!(ask(&mut tty, CELL_AND_ATTRIBUTES), b"\x1b[?62;22c");
        assert_eq!(tty.chunks, [&b"j"[..]]);
    }

    #[test]
    fn probe_reads_sixel_support() {
        let cases: [(&[u8], bool); 7] = [
            (b"\x1b[6;17;8t\x1b[?62;4;22c", true), // foot
            (b"\x1b[?65;4;6;18;22c", true),        // WezTerm
            (b"\x1b[?62;22c", false),              // Ghostty
            (b"\x1b[?1;2c", false),                // tmux without sixel
            (b"\x1b[?6c", false),                  // the Linux console
            (b"\x1b[?4c", false),                  // a class, not an attribute
            (b"", false),                          // no answer
        ];
        for (reply, want) in cases {
            assert_eq!(has_sixel(reply), want, "{reply:?}");
        }
        assert_eq!(cell_reply(b"\x1b[6;17;8t\x1b[?62;4;22c"), Some(CellSize { w: 8.0, h: 17.0 }));
        assert_eq!(color_registers(b"\x1b[?1;0;16S\x1b[?63;4c"), 16);
        assert_eq!(color_registers(b"\x1b[?1;3;0S\x1b[?63;4c"), 0, "a failed query has no registers");
        // herdr accepts the kitty graphics query ahead of its other answers.
        let herdr = b"\x1b_Gi=31;OK\x1b\\\x1b[6;40;20t\x1b[?62;22c";
        assert!(has_kitty(herdr) && !has_sixel(herdr));
        assert_eq!(cell_reply(herdr), Some(CellSize { w: 20.0, h: 40.0 }));
        let refusals: [&[u8]; 3] = [b"\x1b_Gi=31;ENOTSUPPORTED\x1b\\\x1b[?62;22c", b"\x1b[?62;22c", b""];
        for reply in refusals {
            assert!(!has_kitty(reply), "{reply:?} counted as kitty graphics");
        }
    }

    #[test]
    fn device_attributes_skip_other_private_replies() {
        assert!(has_sixel(b"\x1b[?1;0;256S\x1b[?63;4c"));
        assert_eq!(cell_reply(b"\x1b[6;0;8t"), None);
        assert_eq!(cell_reply(b"\x1b[6;17;8;1t"), None);
    }

    #[test]
    fn query_asks_herdr_about_kitty_graphics_first() {
        let mut tty = trickle(&[b"\x1b_Gi=31;OK\x1b\\", b"\x1b[6;40;20t", b"\x1b[?62;22c"]);
        let terminal = query(&mut tty, false, true, false);
        assert_eq!(tty.written, [KITTY_QUERY, CELL_AND_ATTRIBUTES].concat());
        assert_eq!(terminal, Terminal { cell: Some(CellSize { w: 20.0, h: 40.0 }), kitty: true, ..Terminal::default() });
    }

    #[test]
    fn query_asks_a_sixel_terminal_for_color_registers() {
        let mut tty = trickle(&[b"\x1b[6;17;8t\x1b[?62;", b"4;22c", b"\x1b[?1;0;1024S", b"\x1b[?62;4;22c"]);
        let terminal = query(&mut tty, false, false, false);
        assert_eq!(tty.written, [CELL_AND_ATTRIBUTES, COLOR_REGISTERS].concat());
        assert_eq!(terminal, Terminal { cell: Some(CellSize { w: 8.0, h: 17.0 }), sixel: true, colors: 1024, ..Terminal::default() });
    }

    #[test]
    fn background_color_reads_an_osc_11_reply() {
        let cases: [(&[u8], Option<[u8; 3]>); 8] = [
            (b"\x1b]11;rgb:fae0/fae0/fae0\x1b\\", Some([250, 250, 250])), // iTerm2
            (b"\x1b]11;rgb:0000/8000/ffff\x07", Some([0, 128, 255])),
            (b"\x1b]11;rgb:fa/fa/fa\x1b\\", Some([250, 250, 250])),
            (b"\x1b]11;rgb:f/0/80\x07", Some([255, 0, 128])),
            (b"\x1b]11;rgb:fae0/fae0/fa", None),          // cut short
            (b"\x1b]11;rgb:fae0/fae0\x1b\\", None),       // two components
            (b"\x1b]11;rgb:fae00/fae0/fae0\x1b\\", None), // five digits
            (b"", None),                                  // no reply
        ];
        for (reply, want) in cases {
            assert_eq!(background_color(reply), want, "{:?}", String::from_utf8_lossy(reply));
        }
    }

    #[test]
    fn query_asks_iterm2_for_its_background_with_the_color_registers() {
        let mut tty = trickle(&[b"\x1b[6;17;8t\x1b[?62;4c", b"\x1b[?1;0;256S\x1b]11;rgb:fae0/", b"fae0/fae0\x1b\\\x1b[?62;4c"]);
        let terminal = query(&mut tty, false, false, true);
        assert_eq!(tty.written, [CELL_AND_ATTRIBUTES, COLOR_REGISTERS_AND_BACKGROUND].concat());
        assert_eq!((terminal.colors, terminal.background), (256, Some([250, 250, 250])));

        // No sixel questions, no background question.
        let mut tty = trickle(&[b"\x1b[?62;22c"]);
        assert_eq!(query(&mut tty, false, false, true), Terminal::default());
        assert_eq!(tty.written, CELL_AND_ATTRIBUTES);

        // An iTerm2 that does not answer it.
        let mut tty = trickle(&[b"\x1b[?62;4c", b"\x1b[?1;0;256S\x1b[?62;4c"]);
        assert_eq!(query(&mut tty, false, false, true).background, None);
    }

    #[test]
    fn query_asks_for_color_registers_only_when_sixel_is_wanted() {
        let mut tty = trickle(&[b"\x1b[?62;22c"]);
        assert_eq!(query(&mut tty, false, false, false), Terminal::default());
        assert_eq!(tty.written, CELL_AND_ATTRIBUTES);

        let mut tty = trickle(&[b"\x1b[?62;22c", b"\x1b[?1;0;256S\x1b[?62;22c"]);
        assert_eq!(query(&mut tty, true, false, false).colors, 256);
        assert_eq!(tty.written, [CELL_AND_ATTRIBUTES, COLOR_REGISTERS].concat());
    }
}
