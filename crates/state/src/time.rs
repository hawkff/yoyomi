//! Times as bookmarks save them: RFC 3339 text with the UTC offset the time
//! was taken in.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time in whole seconds, with the local UTC offset.
pub(crate) fn now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    local(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// `secs` after the Unix epoch with the local UTC offset of that moment.
pub(crate) fn local(secs: i64) -> String {
    rfc3339(secs, local_offset(secs))
}

/// `secs` after the Unix epoch as RFC 3339 at `offset` seconds east of UTC,
/// whole seconds, `Z` for UTC.
fn rfc3339(secs: i64, offset: i64) -> String {
    let local = secs.saturating_add(offset);
    let (days, time) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let zone = match offset / 60 {
        0 => "Z".to_owned(),
        minutes => format!("{}{:02}:{:02}", if minutes < 0 { '-' } else { '+' }, minutes.abs() / 60, minutes.abs() % 60),
    };
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{zone}", time / 3600, time % 3600 / 60, time % 60)
}

/// The local UTC offset at `secs` after the Unix epoch, in seconds east of
/// UTC, as `localtime_r` gives it for the time zone `TZ` names or, without
/// `TZ`, the zone the system sets.
#[cfg(unix)]
fn local_offset(secs: i64) -> i64 {
    let time = secs as libc::time_t;
    // SAFETY: `tm` is plain integers and a pointer, for which all zeros are valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers come from live locals, and localtime_r writes only `tm`.
    let converted = unsafe { libc::localtime_r(&time, &mut tm) };
    if converted.is_null() { 0 } else { tm.tm_gmtoff as i64 }
}

#[cfg(not(unix))]
fn local_offset(_secs: i64) -> i64 {
    0
}

/// Whether `text` has the shape of an RFC 3339 time: `YYYY-MM-DDTHH:MM:SS`
/// with an uppercase `T`, optional fractional seconds, then an uppercase `Z`
/// or `±HH:MM`, all digits ASCII.
pub(crate) fn is_rfc3339(text: &str) -> bool {
    let fits = |text: &[u8], shape: &[u8]| {
        text.len() == shape.len() && text.iter().zip(shape).all(|(&c, &s)| if s == b'd' { c.is_ascii_digit() } else { c == s })
    };
    let Some((time, mut rest)) = text.as_bytes().split_at_checked(19) else { return false };
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }
        rest = &fraction[digits..];
    }
    let zone = rest == b"Z" || rest.first().is_some_and(|sign| matches!(sign, b'+' | b'-')) && fits(&rest[1..], b"dd:dd");
    fits(time, b"dddd-dd-ddTdd:dd:dd") && zone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_writes_the_civil_time_at_the_offset() {
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400, 0), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000, 0), "2023-11-14T22:13:20Z");
        // 2026-11-12T00:30:00+09:00 is still the 11th in UTC.
        assert_eq!(rfc3339(1_794_411_000, 9 * 3600), "2026-11-12T00:30:00+09:00");
        assert_eq!(rfc3339(1_700_000_000, -(5 * 3600 + 30 * 60)), "2023-11-14T16:43:20-05:30");
    }

    #[test]
    fn local_times_have_the_rfc3339_shape() {
        assert!(is_rfc3339(&now()));
        assert!(is_rfc3339(&local(0)));
    }

    #[test]
    fn is_rfc3339_takes_t_separated_times_with_a_zone_only() {
        for text in ["2026-11-12T09:30:00+09:00", "2026-11-12T09:30:00Z", "2026-11-12T09:30:00.123456789-05:30"] {
            assert!(is_rfc3339(text), "{text}");
        }
        for text in [
            "",
            "2026-11-12",
            "2026-11-12 09:30:00Z",
            "2026-11-12T09:30:00",
            "2026-11-12T09:30:00.Z",
            "2026-11-12T09:30:00+0900",
            "２026-11-12T09:30:00Z",
        ] {
            assert!(!is_rfc3339(text), "{text}");
        }
    }
}
