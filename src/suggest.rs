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

/// Compute the display column (`0..screen_cols`) at the end of the final line of `prompt_bytes`.
pub fn measure_prompt_last_line_col(prompt_bytes: &[u8], screen_cols: usize) -> usize {
    if screen_cols == 0 {
        return 0;
    }

    let last_prompt_line = match prompt_bytes.iter().rposition(|&b| b == b'\n') {
        Some(pos) => &prompt_bytes[pos + 1..],
        None => prompt_bytes,
    };

    let mut col = 0usize;
    measure_prompt_line(last_prompt_line, screen_cols, &mut col);
    if col >= screen_cols { 0 } else { col }
}

/// Compute the number of remaining terminal columns available for ghost text on the
/// current display row, leaving a 1-column right margin to prevent auto-wrap.
///
/// Handles:
/// - Multi-line prompts (only measures from after the final `\n`).
/// - Readline invisible prompt markers (`\x01`..`\x02`) and raw ANSI CSI/OSC escapes.
/// - UTF-8 / CJK / Emoji display widths and terminal line wrapping at `screen_cols`.
pub fn remaining_line_columns(prompt_bytes: &[u8], line: &str, screen_cols: usize) -> usize {
    if screen_cols <= 1 {
        return 0;
    }

    let last_prompt_line = match prompt_bytes.iter().rposition(|&b| b == b'\n') {
        Some(pos) => &prompt_bytes[pos + 1..],
        None => prompt_bytes,
    };

    let mut col = 0usize;
    measure_prompt_line(last_prompt_line, screen_cols, &mut col);

    for ch in line.chars() {
        match ch {
            '\n' | '\r' => col = 0,
            '\t' => {
                let step = 8 - (col % 8);
                advance_col(&mut col, step, screen_cols);
            }
            c if c.is_control() => advance_col(&mut col, 2, screen_cols),
            c => advance_col(&mut col, char_display_width(c), screen_cols),
        }
    }

    screen_cols.saturating_sub(col.saturating_add(1))
}

fn advance_col(col: &mut usize, width: usize, screen_cols: usize) {
    if width == 0 {
        return;
    }
    if *col >= screen_cols {
        *col = 0;
    }
    if *col + width > screen_cols {
        *col = width.min(screen_cols);
    } else {
        *col += width;
    }
}

fn measure_prompt_line(bytes: &[u8], screen_cols: usize, col: &mut usize) {
    let mut i = 0;
    let mut ignoring = false;

    while i < bytes.len() {
        match bytes[i] {
            0x01 => {
                ignoring = true;
                i += 1;
            }
            0x02 => {
                ignoring = false;
                i += 1;
            }
            _ if ignoring => {
                i += 1;
            }
            0x1b => {
                i = skip_ansi_escape(bytes, i);
            }
            b'\r' => {
                *col = 0;
                i += 1;
            }
            b'\t' => {
                let step = 8 - (*col % 8);
                advance_col(col, step, screen_cols);
                i += 1;
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], 0x01 | 0x02 | 0x1b | b'\r' | b'\t') {
                    i += 1;
                }
                for chunk in bytes[start..i].utf8_chunks() {
                    for ch in chunk.valid().chars() {
                        advance_col(col, char_display_width(ch), screen_cols);
                    }
                    if !chunk.invalid().is_empty() {
                        advance_col(col, 1, screen_cols);
                    }
                }
            }
        }
    }
}

fn skip_ansi_escape(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    if i >= bytes.len() {
        return i;
    }
    match bytes[i] {
        // CSI: ESC [ ... final byte 0x40..=0x7E
        b'[' => {
            i += 1;
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7E).contains(&b) {
                    break;
                }
            }
            i
        }
        // OSC: ESC ] ... BEL or ST (ESC \)
        b']' => {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    return i + 2;
                }
                i += 1;
            }
            i
        }
        // Two-byte escape sequence (e.g. ESC =, ESC >)
        _ => i + 1,
    }
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

    #[test]
    fn computes_remaining_columns_for_ascii_cjk_ansi_and_wrapping() {
        // Basic ASCII prompt + input: "$ " (2) + "ap" (2) = 4, cols=14 -> avail=9
        assert_eq!(remaining_line_columns(b"$ ", "ap", 14), 9);

        // CJK input: "$ " (2) + "echo 你好世界" (5 + 8 = 13) = 15 cols, cols=20 -> avail=4
        assert_eq!(remaining_line_columns(b"$ ", "echo 你好世界", 20), 4);

        // Readline \x01..\x02 invisible prompt sequences + multi-line prompt
        let prompt = b"\x01\x1b[32m\x02user@host\x01\x1b[0m\x02\n\x01\x1b[1;34m\x02\xe2\x9d\xaf \x01\x1b[0m\x02";
        // Last line is "❯ " -> 2 display cols + "ls" (2) = 4, cols=20 -> avail=15
        assert_eq!(remaining_line_columns(prompt, "ls", 20), 15);

        // Raw ANSI CSI/OSC escapes without \x01..\x02 markers
        let raw_ansi = b"\x1b]0;title\x07\x1b[36m$ \x1b[0m";
        assert_eq!(remaining_line_columns(raw_ansi, "abc", 10), 4);

        // Wrapping onto next line: cols=10, "$ " (2) + "123456789" (9) -> col=1 on row 2 -> avail=8
        assert_eq!(remaining_line_columns(b"$ ", "123456789", 10), 8);

        // Exact right margin (col == screen_cols) leaves 0 columns to avoid deferred-wrap glitches
        assert_eq!(remaining_line_columns(b"$ ", "12345678", 10), 0);
    }
}
