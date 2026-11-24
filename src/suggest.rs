//! Pure Rust suggestion matching, word splitting, and terminal column width logic.
//! Enforced `#[forbid(unsafe_code)]` at the module boundary in `lib.rs`.

/// Given a non-empty `prefix` and an iterator of raw history byte slices ordered from
/// **newest to oldest**, return a borrowed single-line UTF-8 suffix to suggest.
///
/// Performance & UX rules:
/// - Performs raw byte prefix matching first so non-matching entries never pay for UTF-8
///   validation or heap allocation.
/// - Skips whitespace-only prefixes and multi-line prefixes.
/// - Stops at the first `\n`/`\r` so accepting never executes a command accidentally.
/// - Trims trailing horizontal whitespace (`' '`, `'\t'`) so stale entries like `"agy "`
///   never produce invisible ghost spaces.
pub fn find_suggestion_bytes<'a, I>(prefix: &str, newest_first: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    if prefix.trim().is_empty() || prefix.contains(['\n', '\r']) {
        return None;
    }

    let prefix_bytes = prefix.as_bytes();
    for entry in newest_first {
        let Some(rest) = entry.strip_prefix(prefix_bytes) else {
            continue;
        };

        let first_line = rest
            .split(|&b| b == b'\n' || b == b'\r')
            .next()
            .unwrap_or(&[]);
        let trimmed = trim_end_ascii_blank(first_line);
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(suffix) = std::str::from_utf8(trimmed) {
            return Some(suffix);
        }
    }

    None
}

fn trim_end_ascii_blank(mut bytes: &[u8]) -> &[u8] {
    while let Some(&last) = bytes.last() {
        if last == b' ' || last == b'\t' {
            bytes = &bytes[..bytes.len() - 1];
        } else {
            break;
        }
    }
    bytes
}

/// Extract the next word segment from `suggestion` for partial acceptance (`Alt+→` / `Alt+f`).
///
/// Consumes any leading non-word characters (spaces/punctuation) followed by one
/// contiguous run of word characters (alphanumeric, `_`, `-`).
pub fn next_word_prefix(suggestion: &str) -> &str {
    if suggestion.is_empty() {
        return "";
    }

    let mut seen_word = false;
    for (idx, ch) in suggestion.char_indices() {
        let is_word = is_word_char(ch);
        if is_word {
            seen_word = true;
        } else if seen_word {
            return &suggestion[..idx];
        }
    }

    suggestion
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-')
}

/// Truncate `text` to fit within at most `max_cols` terminal cells without
/// splitting a UTF-8 character or exceeding the screen width.
pub fn truncate_to_columns(text: &str, max_cols: usize) -> &str {
    if max_cols == 0 || text.is_empty() {
        return "";
    }

    let mut cols = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch.is_control() {
            return &text[..idx];
        }
        let w = char_display_width(ch);
        if cols + w > max_cols {
            return &text[..idx];
        }
        cols += w;
    }

    text
}

/// Lightweight East Asian / combining character width estimator (zero external crates).
pub fn char_display_width(ch: char) -> usize {
    let cp = ch as u32;
    if cp == 0 || ch.is_control() {
        return 0;
    }
    // Combining diacritical marks and zero-width joiners/variation selectors.
    if matches!(
        cp,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200F
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
    ) {
        return 0;
    }
    // Common CJK, Hangul, Kana, Fullwidth forms, and Emoji blocks.
    if matches!(
        cp,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0x303E
            | 0x3040..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

/// Validate a user-supplied ANSI SGR style string (e.g. `38;5;242`, `90`, `2;37`).
/// Only digits and `;` are permitted so escape sequences cannot be injected.
pub fn sanitize_sgr_style(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 32 {
        return None;
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit() || b == b';') {
        Some(trimmed)
    } else {
        None
    }
}

/// Check whether `TERM` supports ANSI escape sequences for ghost text.
pub fn is_supported_term(term: Option<&str>) -> bool {
    matches!(term, Some(t) if !t.is_empty() && !t.eq_ignore_ascii_case("dumb"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(prefix: &str, hist: &'a [&'a str]) -> Option<&'a str> {
        find_suggestion_bytes(prefix, hist.iter().rev().map(|s| s.as_bytes()))
    }

    #[test]
    fn matches_newest_history_prefix() {
        let hist = ["apt update", "agy whoami", "agy", "apt upgrade"];
        assert_eq!(find("a", &hist), Some("pt upgrade"));
        assert_eq!(find("ag", &hist), Some("y"));
        assert_eq!(find("agy ", &hist), Some("whoami"));
    }

    #[test]
    fn skips_trailing_whitespace_and_exact_matches() {
        let hist = ["agy whoami   ", "agy   ", "agy"];
        assert_eq!(find("agy", &hist), Some(" whoami"));
        assert_eq!(find("agy whoami", &hist), None);
    }

    #[test]
    fn strips_multiline_and_ignores_invalid_prefix() {
        let hist = ["agy\necho danger", "agy whoami\necho next"];
        assert_eq!(find("a", &hist), Some("gy whoami"));
        assert_eq!(find("", &hist), None);
        assert_eq!(find("   ", &hist), None);
        assert_eq!(find("a\nb", &hist), None);
    }

    #[test]
    fn skips_invalid_utf8_history_gracefully() {
        let hist: [&[u8]; 2] = [b"agy ok", b"agy \xff\xfe"];
        assert_eq!(
            find_suggestion_bytes("a", hist.into_iter().rev()),
            Some("gy ok")
        );
    }

    #[test]
    fn next_word_prefix_steps_cleanly() {
        assert_eq!(next_word_prefix("gy whoami --json"), "gy");
        assert_eq!(next_word_prefix(" whoami --json"), " whoami");
        assert_eq!(next_word_prefix(" --json"), " --json");
        assert_eq!(next_word_prefix("   "), "   ");
        assert_eq!(next_word_prefix(""), "");
    }

    #[test]
    fn truncates_ascii_and_cjk_safely() {
        assert_eq!(truncate_to_columns("hello world", 5), "hello");
        assert_eq!(truncate_to_columns("hello", 0), "");
        assert_eq!(truncate_to_columns("你好世界", 5), "你好");
        assert_eq!(truncate_to_columns("a你好", 3), "a你");
        assert_eq!(truncate_to_columns("a你好", 2), "a");
    }

    #[test]
    fn validates_sgr_style_and_term() {
        assert_eq!(sanitize_sgr_style("38;5;242"), Some("38;5;242"));
        assert_eq!(sanitize_sgr_style(" 90 "), Some("90"));
        assert_eq!(sanitize_sgr_style("38;5;242m\x1b[2J"), None);
        assert_eq!(sanitize_sgr_style(""), None);

        assert!(is_supported_term(Some("xterm-256color")));
        assert!(!is_supported_term(Some("dumb")));
        assert!(!is_supported_term(Some("")));
        assert!(!is_supported_term(None));
    }
}
