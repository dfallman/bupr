//! Human-readable formatting.

use std::path::Path;

pub fn bytes(n: u64) -> String {
    bytesize::ByteSize::b(n).display().si().to_string()
}

pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn relative(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", d / 86_400),
    }
}

pub fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Fit `s` into `max` characters: `first/…/last` if that fits, else `…tail`.
pub fn shorten_middle(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if let (Some((first, _)), Some((_, last))) = (s.split_once('/'), s.rsplit_once('/')) {
        let candidate = format!("{first}/…/{last}");
        if candidate.chars().count() <= max {
            return candidate;
        }
    }
    let tail: String = s.chars().skip(len - (max - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_numbers() {
        assert_eq!(count(14002), "14,002");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(bytes(5_400_000_000), "5.4 GB");
        assert_eq!(bytes(0), "0 B");
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(22), "0:22");
        assert_eq!(clock(622), "10:22");
        assert_eq!(clock(3723), "1:02:03");
    }

    #[test]
    fn relative_times() {
        let now = 1_000_000;
        assert_eq!(relative(now, now - 30), "just now");
        assert_eq!(relative(now, now - 600), "10 min ago");
        assert_eq!(relative(now, now - 7200), "2 h ago");
        assert_eq!(relative(now, now - 86_400 - 60), "yesterday");
        assert_eq!(relative(now, now - 3 * 86_400), "3 days ago");
    }

    #[test]
    fn tilde_abbreviates_home() {
        let h = Path::new("/Users/me");
        assert_eq!(tilde(Path::new("/Users/me/dev"), h), "~/dev");
        assert_eq!(tilde(Path::new("/Users/me"), h), "~");
        assert_eq!(tilde(Path::new("/Volumes/B"), h), "/Volumes/B");
    }

    #[test]
    fn shorten_middle_keeps_first_and_last_components() {
        let p = "webshop/src/lib/components/Timeline.svelte";
        assert_eq!(shorten_middle(p, 60), p);
        assert_eq!(shorten_middle(p, 30), "webshop/…/Timeline.svelte");
        assert_eq!(shorten_middle(p, 12), "…line.svelte");
        assert_eq!(shorten_middle("x", 0), "");
    }
}
