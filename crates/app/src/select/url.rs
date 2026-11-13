//! Tracking parameters removed from links before they open or copy.

/// Query keys that only track clicks.
const TRACKING_KEYS: &[&str] = &[
    "fbclid",
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "twclid",
    "ttclid",
    "igshid",
    "igsh",
    "yclid",
    "ysclid",
    "srsltid",
    "si",
    "mkt_tok",
    "ml_subscriber",
    "ml_subscriber_hash",
    "rb_clickid",
    "s_cid",
    "s_kwcid",
    "wickedid",
    "_openstat",
    "ref_src",
    "ref_url",
    "spm",
    "scm",
    "_branch_match_id",
    "_kx",
    "epik",
    "li_fat_id",
    "cmpid",
    "ncid",
    "_ga",
    "_gl",
];

/// Prefixes of query keys that only track clicks.
const TRACKING_PREFIXES: &[&str] = &["utm_", "mtm_", "pk_", "piwik_", "hsa_", "_hs", "__hs", "mc_", "oly_", "vero_"];

/// `raw` without its tracking parameters. The other fields keep their order
/// and spelling, and a query left empty drops its `?`.
pub(crate) fn clean_url(raw: &str) -> String {
    let (rest, fragment) = raw.split_at(raw.find('#').unwrap_or(raw.len()));
    let Some((base, query)) = rest.split_once('?').filter(|(_, query)| !query.is_empty()) else { return raw.to_owned() };
    let kept: Vec<&str> = query.split('&').filter(|pair| !tracking(pair.split('=').next().unwrap_or_default())).collect();
    if kept.is_empty() { format!("{base}{fragment}") } else { format!("{base}?{}{fragment}", kept.join("&")) }
}

fn tracking(key: &str) -> bool {
    let key = unescape(key).unwrap_or_else(|| key.to_owned()).to_lowercase();
    TRACKING_KEYS.contains(&key.as_str()) || TRACKING_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Decodes a query key: `%XX` escapes and `+` for a space. `None` when an
/// escape is broken.
fn unescape(key: &str) -> Option<String> {
    let bytes = key.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))?;
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_parameters_drop_out() {
        assert_eq!(clean_url("https://example.com/a?utm_source=pdf"), "https://example.com/a");
        assert_eq!(clean_url("https://example.com/a?id=7&utm_medium=x&fbclid=1&b=2#top"), "https://example.com/a?id=7&b=2#top");
        assert_eq!(clean_url("https://youtu.be/x?SI=abc&t=10"), "https://youtu.be/x?t=10", "keys compare without case");
        assert_eq!(clean_url("https://example.com/?%75tm_source=x&q=a+b"), "https://example.com/?q=a+b", "escaped keys");
        assert_eq!(clean_url("https://example.com/?%zz=1&gclid=2"), "https://example.com/?%zz=1");
        assert_eq!(clean_url("https://example.com/#?utm_source=x"), "https://example.com/#?utm_source=x", "a fragment is no query");
        assert_eq!(clean_url("mailto:me@example.com"), "mailto:me@example.com");
    }
}
