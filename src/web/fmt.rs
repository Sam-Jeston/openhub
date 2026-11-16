//! Small formatting helpers called from templates.

use chrono::{DateTime, Utc};

use crate::db::now;

/// Lets helpers accept numbers by value or by (nested) reference, since
/// templates pass fields by reference but method results by value.
pub trait Num<T: Copy> {
    fn value(&self) -> T;
}

impl Num<i64> for i64 {
    fn value(&self) -> i64 {
        *self
    }
}

impl Num<usize> for usize {
    fn value(&self) -> usize {
        *self
    }
}

impl<T: Copy, N: Num<T> + ?Sized> Num<T> for &N {
    fn value(&self) -> T {
        (**self).value()
    }
}

pub fn icon(name: &str) -> String {
    crate::icons::icon(name)
}

/// Integer counts, in whichever width a template happens to pass.
pub trait Count: Copy {
    fn count(self) -> i64;
}

impl Count for i64 {
    fn count(self) -> i64 {
        self
    }
}

impl Count for usize {
    fn count(self) -> i64 {
        self as i64
    }
}

impl<T: Count + ?Sized> Count for &T {
    fn count(self) -> i64 {
        (*self).count()
    }
}

/// "3 days ago" style relative time.
pub fn ago(ts: impl Num<i64>) -> String {
    let delta = now() - ts.value();
    if delta < 45 {
        return "just now".into();
    }
    let (n, unit) = match delta {
        d if d < 3600 => ((d + 30) / 60, "minute"),
        d if d < 86400 => ((d + 1800) / 3600, "hour"),
        d if d < 30 * 86400 => ((d + 43200) / 86400, "day"),
        d if d < 365 * 86400 => (d / (30 * 86400), "month"),
        d => (d / (365 * 86400), "year"),
    };
    let n = n.max(1);
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn datetime(ts: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(ts, 0).unwrap_or_default()
}

/// "Oct 10, 2026"
pub fn date(ts: impl Num<i64>) -> String {
    datetime(ts.value()).format("%b %-d, %Y").to_string()
}

/// "Oct 10, 2026, 14:05 UTC"
pub fn datetime_full(ts: impl Num<i64>) -> String {
    datetime(ts.value())
        .format("%b %-d, %Y, %H:%M UTC")
        .to_string()
}

pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

pub fn bytes(n: impl Num<usize>) -> String {
    let n = n.value() as f64;
    if n < 1024.0 {
        format!("{n} Bytes")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else {
        format!("{:.1} MB", n / (1024.0 * 1024.0))
    }
}

/// "1 commit", "3 commits". Accepts `usize` counts such as commit totals.
pub fn plural_size(n: impl Count, word: &str) -> String {
    plural(n, word)
}

/// "1 issue", "3 issues". Accepts `i64` counts such as row counts.
pub fn plural(n: impl Count, word: &str) -> String {
    let n = n.count();
    if n == 1 {
        format!("1 {word}")
    } else if word.ends_with("ch") || word.ends_with('s') || word.ends_with('x') {
        format!("{n} {word}es")
    } else {
        format!("{n} {word}s")
    }
}

/// A coloured initial badge used in place of profile pictures.
pub fn avatar(name: &str, size: u32) -> String {
    let hue = name
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32))
        % 360;
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    let font = (size as f32 * 0.45).round() as u32;
    format!(
        r#"<span class="avatar" style="--hue:{hue};width:{size}px;height:{size}px;font-size:{font}px" aria-hidden="true">{}</span>"#,
        crate::highlight::escape(&initial)
    )
}

/// Percent-encodes a value for use in a query string.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encodes a repository path or ref for use in a URL path, keeping `/`.
pub fn urlpath(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b'@'
            | b'+'
            | b'!'
            | b'$'
            | b'('
            | b')'
            | b','
            | b';'
            | b'='
            | b':' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        let n = now();
        assert_eq!(ago(n), "just now");
        assert_eq!(ago(n - 60), "1 minute ago");
        assert_eq!(ago(n - 2 * 3600), "2 hours ago");
        assert_eq!(ago(n - 3 * 86400), "3 days ago");
    }

    #[test]
    fn url_encoding() {
        assert_eq!(urlpath("src/a b#c.rs"), "src/a%20b%23c.rs");
        assert_eq!(urlencode("/a/b?c"), "%2Fa%2Fb%3Fc");
    }
}
