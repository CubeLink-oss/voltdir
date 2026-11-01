//! 時間格式：RFC 3339，UTC，結尾 `Z`，小數秒最多 7 位並去除結尾的 0。

use std::io::Write;

const TICKS_PER_SEC: u64 = 10_000_000;
/// 1601-01-01 到 1970-01-01 的天數。
const DAYS_1601_TO_1970: i64 = 134_774;

/// 把 FILETIME（自 1601-01-01 起的 100 奈秒數）格式化後附加到 `out`。
pub fn push_filetime(out: &mut Vec<u8>, filetime: u64) {
    let secs = filetime / TICKS_PER_SEC;
    let frac = filetime % TICKS_PER_SEC;
    let days = (secs / 86_400) as i64 - DAYS_1601_TO_1970;
    let sod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let _ = write!(
        out,
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    );
    if frac != 0 {
        let digits = format!("{frac:07}");
        let _ = write!(out, ".{}", digits.trim_end_matches('0'));
    }
    out.push(b'Z');
}

/// 解析本工具輸出的 RFC 3339（UTC、結尾 `Z`、小數秒最多 7 位）為 FILETIME。
pub fn parse_filetime(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || *b.last()? != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let frac = match b.get(19)? {
        b'Z' if b.len() == 20 => 0,
        b'.' if b.len() > 21 && b.len() <= 28 => {
            let digits = &b[20..b.len() - 1];
            if !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let mut v: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
            for _ in digits.len()..7 {
                v *= 10;
            }
            v
        }
        _ => return None,
    };
    let days = days_from_civil(y, m as u32, d as u32) + DAYS_1601_TO_1970;
    if days < 0 {
        return None;
    }
    let secs = days as u64 * 86_400 + (hh * 3600 + mm * 60 + ss) as u64;
    Some(secs * TICKS_PER_SEC + frac)
}

/// 公曆年月日轉為自 1970-01-01 起的天數（Howard Hinnant 的公開日期演算法）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 把 Unix 時間（秒、奈秒）轉成 FILETIME。
pub fn unix_to_filetime(secs: u64, nanos: u32) -> u64 {
    (secs + (DAYS_1601_TO_1970 as u64) * 86_400) * TICKS_PER_SEC + u64::from(nanos) / 100
}

/// 目前時間的 FILETIME。
pub fn now_filetime() -> u64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    unix_to_filetime(d.as_secs(), d.subsec_nanos())
}

/// 自 1970-01-01 起的天數轉為公曆年月日（Howard Hinnant 的公開日期演算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(ft: u64) -> String {
        let mut v = Vec::new();
        push_filetime(&mut v, ft);
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn epoch_and_whole_seconds_have_no_fraction() {
        assert_eq!(fmt(0), "1601-01-01T00:00:00Z");
        assert_eq!(fmt(unix_to_filetime(0, 0)), "1970-01-01T00:00:00Z");
        assert_eq!(
            fmt(unix_to_filetime(1_700_000_000, 0)),
            "2023-11-14T22:13:20Z"
        );
    }

    #[test]
    fn fraction_is_trimmed_of_trailing_zeros() {
        let base = unix_to_filetime(1_700_000_000, 0);
        assert_eq!(fmt(base + 5_000_000), "2023-11-14T22:13:20.5Z");
        assert_eq!(fmt(base + 1), "2023-11-14T22:13:20.0000001Z");
        assert_eq!(fmt(base + 1_234_500), "2023-11-14T22:13:20.12345Z");
        assert_eq!(fmt(base + 9_999_999), "2023-11-14T22:13:20.9999999Z");
    }

    #[test]
    fn parse_is_the_inverse_of_format() {
        for ft in [
            0u64,
            unix_to_filetime(0, 0),
            unix_to_filetime(1_700_000_000, 0) + 1,
            unix_to_filetime(1_709_164_800, 0) + 1_234_500,
            unix_to_filetime(4_107_542_400, 0) + 9_999_999,
        ] {
            assert_eq!(parse_filetime(&fmt(ft)), Some(ft), "{}", fmt(ft));
        }
        assert_eq!(
            parse_filetime("2023-11-14T22:13:20.5Z"),
            Some(unix_to_filetime(1_700_000_000, 500_000_000))
        );
    }

    #[test]
    fn parse_rejects_malformed_times() {
        for bad in [
            "",
            "2023-11-14",
            "2023-11-14T22:13:20",
            "2023-13-14T22:13:20Z",
            "2023-11-14T25:13:20Z",
            "2023-11-14T22:13:20.Z",
            "2023-11-14T22:13:20.123456789Z",
            "2023/11/14T22:13:20Z",
            "abcd-11-14T22:13:20Z",
        ] {
            assert_eq!(parse_filetime(bad), None, "{bad}");
        }
    }

    #[test]
    fn leap_days_and_century_rules() {
        assert_eq!(
            fmt(unix_to_filetime(951_782_400, 0)),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            fmt(unix_to_filetime(4_107_542_400, 0)),
            "2100-03-01T00:00:00Z"
        );
        assert_eq!(
            fmt(unix_to_filetime(1_709_164_800, 0)),
            "2024-02-29T00:00:00Z"
        );
    }
}
