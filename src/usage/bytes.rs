//! Fields read straight from a log's bytes. A first scan touches hundreds of
//! thousands of rows; parsing each one into a value is what made a scan slow
//! and large (Nenu measured 170 MB against 34 MB without), so the hot path
//! finds numbers and ids in place and allocates only for names that repeat.

use memchr::memmem;

const QUOTE: u8 = b'"';
const BACKSLASH: u8 = b'\\';

/// Where `needle` first starts in `b[from..to]`.
pub fn find(b: &[u8], needle: &memmem::Finder, from: usize, to: usize) -> Option<usize> {
    let to = to.min(b.len());
    if from >= to {
        return None;
    }
    needle.find(&b[from..to]).map(|i| from + i)
}

/// Where `needle` last starts in `b[from..to]`.
pub fn rfind(b: &[u8], needle: &memmem::FinderRev, from: usize, to: usize) -> Option<usize> {
    let to = to.min(b.len());
    if from >= to {
        return None;
    }
    needle.rfind(&b[from..to]).map(|i| from + i)
}

/// Index of the `}` or `]` that closes the one opened at `open`, if it comes
/// before `to`.
pub fn object_end(b: &[u8], open: usize, to: usize) -> Option<usize> {
    let mut depth = 0u32;
    let mut in_string = false;
    let mut i = open;
    while i < to.min(b.len()) {
        let c = b[i];
        if in_string {
            if c == BACKSLASH {
                i += 1;
            } else if c == QUOTE {
                in_string = false;
            }
        } else if c == QUOTE {
            in_string = true;
        } else if c == b'{' || c == b'[' {
            depth += 1;
        } else if c == b'}' || c == b']' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The span of the object whose `{` is at `at`.
pub fn object_at(b: &[u8], at: Option<usize>) -> Option<(usize, usize)> {
    let at = at.filter(|&at| b.get(at) == Some(&b'{'))?;
    object_end(b, at, b.len()).map(|end| (at, end))
}

/// The bytes of the string whose opening quote is at `at`, as written.
pub fn string_at(b: &[u8], at: Option<usize>) -> Option<&[u8]> {
    let at = at.filter(|&at| b.get(at) == Some(&QUOTE))?;
    string_end(b, at + 1, b.len()).map(|end| &b[at + 1..end])
}

/// End (exclusive) of the JSON string whose first character is at `from`.
pub fn string_end(b: &[u8], from: usize, to: usize) -> Option<usize> {
    let mut i = from;
    while i < to.min(b.len()) {
        match b[i] {
            BACKSLASH => i += 1,
            QUOTE => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The non-negative integer starting at `at`, or 0 when there is none (JSON
/// `null`, a string...) or it is past 2^53, which no token count reaches:
/// garbage in a log must not add up past `u64`.
pub fn uint_at(b: &[u8], at: usize) -> u64 {
    const MAX: u64 = (1 << 53) - 1;
    let mut n = 0u64;
    for &c in b.get(at..).unwrap_or_default() {
        if !c.is_ascii_digit() {
            break;
        }
        n = n * 10 + u64::from(c - b'0');
        if n > MAX {
            return 0;
        }
    }
    n
}

pub fn starts_with_at(b: &[u8], at: usize, key: &[u8]) -> bool {
    b.get(at..at + key.len()) == Some(key)
}

/// Where the value of each of `keys` starts among the fields directly inside
/// the object spanning `[open, close]`. Keys of nested objects do not match,
/// so a field that also appears deeper (`usage.iterations[].input_tokens`)
/// cannot be taken for the top one.
pub fn top_level_fields<const N: usize>(
    b: &[u8],
    open: usize,
    close: usize,
    keys: &[&[u8]; N],
) -> [Option<usize>; N] {
    let mut out = [None; N];
    let mut depth = 0i32;
    let mut i = open;
    while i <= close {
        let c = b[i];
        if c == QUOTE {
            let Some(end) = string_end(b, i + 1, close) else {
                return out;
            };
            // A string at depth 1 followed by a colon is a key of this object.
            if depth == 1 && b.get(end + 1) == Some(&b':') {
                let key = &b[i + 1..end];
                for (k, slot) in keys.iter().zip(out.iter_mut()) {
                    if slot.is_none() && key == *k {
                        let mut at = end + 2;
                        while b.get(at).is_some_and(|c| c.is_ascii_whitespace()) {
                            at += 1;
                        }
                        *slot = Some(at);
                    }
                }
            }
            i = end;
        } else if c == b'{' || c == b'[' {
            depth += 1;
        } else if c == b'}' || c == b']' {
            depth -= 1;
        }
        i += 1;
    }
    out
}

/// FNV-1a over the bytes of `a` then `b`; never 0, which marks "no key".
pub fn hash(a: &[u8], b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in a.iter().chain(b) {
        h = (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h.max(1)
}

/// Unix milliseconds of an ISO-8601 UTC time (`2026-10-16T12:34:56.789Z`)
/// starting at `at`.
pub fn iso_ms_at(b: &[u8], at: usize) -> Option<i64> {
    let s = b.get(at..at + 19)?;
    let d = |from: usize, n: usize| -> Option<i64> {
        s[from..from + n].iter().try_fold(0i64, |v, &c| {
            c.is_ascii_digit().then(|| v * 10 + i64::from(c - b'0'))
        })
    };
    if s[4] != b'-' || s[10] != b'T' {
        return None;
    }
    let (month, day) = (d(5, 2)?, d(8, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let ms = match b.get(at + 19..at + 23) {
        Some([b'.', m @ ..]) => m.iter().try_fold(0i64, |v, &c| {
            c.is_ascii_digit().then(|| v * 10 + i64::from(c - b'0'))
        })?,
        _ => 0,
    };
    let days = crate::registry::days_from_civil(d(0, 4)?, month as u32, day as u32);
    let secs = days * 86_400 + d(11, 2)? * 3600 + d(14, 2)? * 60 + d(17, 2)?;
    Some(secs * 1000 + ms)
}

pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_fields_skip_nested_keys() {
        let b = br#"{"a":{"x":1},"x": 2,"y":[{"x":3}]}"#;
        let [x, y] = top_level_fields(b, 0, b.len() - 1, &[b"x", b"y"]);
        assert_eq!(uint_at(b, x.unwrap()), 2);
        assert_eq!(b[y.unwrap()], b'[');
    }

    #[test]
    fn iso_times() {
        let t = |s: &str| iso_ms_at(s.as_bytes(), 0);
        assert_eq!(t("1970-01-01T00:00:01.250Z"), Some(1250));
        assert_eq!(t("2026-01-02T03:04:05Z"), Some(1_767_323_045_000));
        assert_eq!(t("2026-13-02T03:04:05Z"), None);
        assert_eq!(t("2026-01-02 03:04:05Z"), None);
    }

    #[test]
    fn strings_and_objects_honour_escapes() {
        let b = br#"{"a":"x\"}","b":{}}"#;
        assert_eq!(object_end(b, 0, b.len()), Some(b.len() - 1));
        assert_eq!(string_end(b, 6, b.len()), Some(10));
    }
}
