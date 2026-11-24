//! Pure Rust Bash syntax highlighter, PATH executable cache, and terminal geometry helpers.
//! Enforced `#[forbid(unsafe_code)]` at the module boundary in `lib.rs`.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::suggest::{char_display_width, measure_prompt_last_line_col};

/// Maximum line length (in bytes) we attempt to syntax-highlight.
/// Keeps huge pasted commands instantaneous.
pub const MAX_HIGHLIGHT_LEN: usize = 2048;

/// Maximum number of filesystem `stat` checks performed per line for path underlining.
const MAX_PATH_CHECKS_PER_LINE: usize = 24;

/// Maximum cached `$PATH` command lookup entries (bounded memory).
const MAX_EXEC_CACHE_ENTRIES: usize = 512;

/// Token highlight category mapped to a standard 16-color ANSI SGR code
/// (adapts cleanly to every terminal color scheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Default,
    ValidCommand,
    InvalidCommand,
    ReservedWord,
    Operator,
    StringLiteral,
    Variable,
    Option,
    ExistingPath,
    Comment,
}

impl TokenKind {
    /// ANSI SGR parameter sequence (without `\x1b[` and `m`), or `None` for default color.
    pub const fn sgr(self) -> Option<&'static [u8]> {
        match self {
            Self::Default => None,
            Self::ValidCommand => Some(b"32"),
            Self::InvalidCommand => Some(b"31"),
            Self::ReservedWord => Some(b"35"),
            Self::Operator => Some(b"35"),
            Self::StringLiteral => Some(b"33"),
            Self::Variable => Some(b"36"),
            Self::Option => Some(b"36"),
            Self::ExistingPath => Some(b"4"),
            Self::Comment => Some(b"90"),
        }
    }
}

/// A highlighted byte span `[start..end)` within the input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Abstraction over Bash command & filesystem resolution so the highlighter stays
/// 100% pure Rust and unit-testable without a live Bash process.
pub trait CommandResolver {
    fn command_exists(&mut self, word: &str) -> bool;
    fn path_exists(&mut self, word: &str) -> bool;
}

/// Lazy, bounded `$PATH` executable cache with zero startup directory scanning.
///
/// Tracks deduplicated `$PATH` directories by `(dev, ino)` so symlinks like `/bin -> /usr/bin`
/// are never probed twice, and invalidates in O(1) whenever `$PATH` or any directory's `mtime`
/// changes (checked once per prompt in `rl_pre_input_hook`).
pub struct PathExecCache {
    path_env: String,
    dirs: Vec<(PathBuf, Option<SystemTime>)>,
    exec_cache: HashMap<String, bool>,
    path_exists_cache: HashMap<String, bool>,
    probe_buf: PathBuf,
}

impl PathExecCache {
    pub fn new() -> Self {
        Self {
            path_env: String::new(),
            dirs: Vec::new(),
            exec_cache: HashMap::new(),
            path_exists_cache: HashMap::new(),
            probe_buf: PathBuf::with_capacity(128),
        }
    }

    /// Clear per-prompt filesystem path existence cache (called on `rl_pre_input_hook`).
    pub fn invalidate_prompt_cache(&mut self) {
        self.path_exists_cache.clear();
    }

    /// Refresh `$PATH` directory list and invalidate cached lookups in O(1) if `$PATH`
    /// or any directory `mtime` changed (zero `readdir()` calls).
    pub fn refresh_if_needed(&mut self) {
        let current_path = std::env::var("PATH").unwrap_or_default();
        let path_changed = current_path != self.path_env;

        if !path_changed && !self.dirs.is_empty() {
            let mut mtime_changed = false;
            for (dir, prev_mtime) in &mut self.dirs {
                let now_mtime = fs::metadata(&*dir).and_then(|m| m.modified()).ok();
                if now_mtime != *prev_mtime {
                    *prev_mtime = now_mtime;
                    mtime_changed = true;
                }
            }
            if mtime_changed {
                self.exec_cache.clear();
            }
            return;
        }

        self.path_env = current_path;
        self.dirs.clear();
        self.exec_cache.clear();

        let mut seen_inodes: Vec<(u64, u64)> = Vec::new();
        for dir_str in self.path_env.split(':').filter(|s| !s.is_empty()) {
            let dir = PathBuf::from(dir_str);
            let Ok(meta) = fs::metadata(&dir) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let dev_ino = (meta.dev(), meta.ino());
            if seen_inodes.contains(&dev_ino) {
                continue;
            }
            seen_inodes.push(dev_ino);
            self.dirs.push((dir, meta.modified().ok()));
        }
    }

    /// Check whether `name` is an executable in `$PATH` (lazily cached) or an executable path.
    pub fn has_executable(&mut self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if name.contains('/') {
            let expanded = expand_tilde(name);
            return is_executable_file(&expanded);
        }
        if let Some(&cached) = self.exec_cache.get(name) {
            return cached;
        }
        if self.dirs.is_empty() && self.path_env.is_empty() {
            self.refresh_if_needed();
        }

        let mut found = false;
        for (dir, _) in &self.dirs {
            self.probe_buf.clear();
            self.probe_buf.push(dir);
            self.probe_buf.push(name);
            if is_executable_file(&self.probe_buf) {
                found = true;
                break;
            }
        }

        if self.exec_cache.len() >= MAX_EXEC_CACHE_ENTRIES {
            self.exec_cache.clear();
        }
        self.exec_cache.insert(name.to_owned(), found);
        found
    }

    /// Cached filesystem existence check for an argument token (capped per prompt).
    pub fn check_path_exists(&mut self, token: &str) -> bool {
        if token.is_empty()
            || token.len() > 256
            || token.contains("://")
            || token.contains(['*', '?', '[', '$', '`'])
        {
            return false;
        }
        if let Some(&cached) = self.path_exists_cache.get(token) {
            return cached;
        }
        let exists = expand_tilde(token).exists();
        if self.path_exists_cache.len() < 128 {
            self.path_exists_cache.insert(token.to_owned(), exists);
        }
        exists
    }
}

impl Default for PathExecCache {
    fn default() -> Self {
        Self::new()
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111) != 0
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

/// Check if `word` is a Bash reserved word.
pub fn is_reserved_word(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "case"
            | "esac"
            | "for"
            | "select"
            | "while"
            | "until"
            | "do"
            | "done"
            | "in"
            | "function"
            | "time"
            | "coproc"
            | "!"
            | "{"
            | "}"
            | "[["
            | "]]"
    )
}

/// Reserved words after which the next word begins a new command.
fn reserved_word_starts_command(word: &str) -> bool {
    matches!(
        word,
        "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "time" | "coproc" | "!" | "{"
    )
}

/// Precommand wrappers whose following command argument should also be highlighted as a command.
fn is_precommand(word: &str) -> bool {
    matches!(
        word,
        "sudo" | "doas" | "env" | "command" | "builtin" | "exec" | "nohup" | "nice" | "strace"
    )
}

/// `sudo`/`doas`/`nice`/`env` short flags that consume the immediately following argument.
fn precommand_flag_takes_arg(precmd: &str, flag: &str) -> bool {
    match precmd {
        "sudo" => matches!(
            flag,
            "-u" | "-g" | "-C" | "-h" | "-p" | "-r" | "-t" | "-T" | "-U"
        ),
        "doas" => matches!(flag, "-u" | "-C"),
        "env" => matches!(flag, "-u" | "-C" | "-S" | "--unset" | "--chdir"),
        "nice" => matches!(flag, "-n" | "--adjustment"),
        _ => false,
    }
}

/// Returns `true` if `word` has valid `NAME=...` or `NAME+=...` Bash assignment syntax.
fn split_assignment(word: &str) -> Option<usize> {
    let bytes = word.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'=' {
            return Some(i);
        }
        if b == b'+' && i + 1 < bytes.len() && bytes[i + 1] == b'=' {
            return Some(i);
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        i += 1;
    }
    None
}

/// Check whether `word` is an unquoted numeric file descriptor followed by a redirection,
/// handled directly by operator scanning.
fn is_all_ascii_digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

/// Parse `BASH_AUTOSUGGEST_HIGHLIGHT` boolean-ish env values (default `true`).
pub fn parse_bool_env(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no" | "disable" | "disabled"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Command,
    Precommand(&'static str),
    PrecommandFlagArg(&'static str),
    RedirectTarget,
    Argument,
}

/// Tokenize and highlight a single-line Bash command into non-overlapping `spans`.
/// Reuses `spans` capacity to avoid per-keystroke allocations.
pub fn highlight_line(line: &str, resolver: &mut impl CommandResolver, spans: &mut Vec<Span>) {
    spans.clear();
    if line.is_empty() || line.len() > MAX_HIGHLIGHT_LEN {
        return;
    }

    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut pos = Position::Command;
    let mut path_checks = 0usize;

    while i < len {
        // 1. Whitespace.
        if bytes[i].is_ascii_whitespace() {
            let start = i;
            while i < len && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            push_span(spans, start, i, TokenKind::Default);
            continue;
        }

        // 2. Comment (`#` at word boundary).
        if bytes[i] == b'#' {
            push_span(spans, i, len, TokenKind::Comment);
            break;
        }

        // 3. Control & grouping operators.
        if let Some((op_len, next_pos)) = match_operator(bytes, i) {
            push_span(spans, i, i + op_len, TokenKind::Operator);
            i += op_len;
            pos = next_pos;
            continue;
        }

        // 4. Optional numeric fd prefix directly before redirection (`2>&1`, `2>/dev/null`, `1>>log`).
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < len && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < len
                && matches!(bytes[j], b'>' | b'<')
                && let Some((redir_len, _)) = match_redirection(bytes, j)
            {
                let end = j + redir_len;
                push_span(spans, i, end, TokenKind::Operator);
                i = end;
                // Only expect a filename target if not already an fd duplication like `2>&1`.
                if !is_fd_dup(&bytes[j..end]) {
                    pos = Position::RedirectTarget;
                }
                continue;
            }
        }

        // 5. Redirection operators (`>`, `>>`, `<`, `<<`, `<<<`, `&>`, `&>>`, `<>`, `>&`, `<&`).
        if let Some((redir_len, expects_target)) = match_redirection(bytes, i) {
            push_span(spans, i, i + redir_len, TokenKind::Operator);
            i += redir_len;
            if expects_target {
                pos = Position::RedirectTarget;
            }
            continue;
        }

        // 6. Regular shell word (may contain quotes, escapes, and `$VAR` expansions).
        let word_start = i;
        let word_end = scan_word_end(bytes, i);
        let word = &line[word_start..word_end];
        i = word_end;

        let has_quotes_or_var = word
            .as_bytes()
            .iter()
            .any(|&b| matches!(b, b'\'' | b'"' | b'$'));

        // Handle `VAR=val` assignments in command / precommand position (`FOO=1 cmd`, `env A=2 cmd`).
        if matches!(pos, Position::Command | Position::Precommand(_))
            && let Some(eq_idx) = split_assignment(word)
        {
            let name_end = word_start + eq_idx;
            let op_end = if bytes[name_end] == b'+' {
                name_end + 2
            } else {
                name_end + 1
            };
            push_span(spans, word_start, name_end, TokenKind::Variable);
            push_span(spans, name_end, op_end, TokenKind::Operator);
            if op_end < word_end {
                emit_word_subtokens(line, op_end, word_end, TokenKind::Default, spans);
            }
            // Stay in Command / Precommand position!
            continue;
        }

        match pos {
            Position::Command => {
                if !has_quotes_or_var && is_reserved_word(word) {
                    push_span(spans, word_start, word_end, TokenKind::ReservedWord);
                    pos = if reserved_word_starts_command(word) {
                        Position::Command
                    } else {
                        Position::Argument
                    };
                } else if has_quotes_or_var {
                    emit_word_subtokens(line, word_start, word_end, TokenKind::Default, spans);
                    pos = Position::Argument;
                } else {
                    let valid = resolver.command_exists(word);
                    let kind = if valid {
                        TokenKind::ValidCommand
                    } else {
                        TokenKind::InvalidCommand
                    };
                    push_span(spans, word_start, word_end, kind);
                    pos = if valid && is_precommand(word) {
                        Position::Precommand(static_precommand_name(word))
                    } else {
                        Position::Argument
                    };
                }
            }
            Position::Precommand(precmd) => {
                if !has_quotes_or_var && word.starts_with('-') && word.len() > 1 {
                    push_span(spans, word_start, word_end, TokenKind::Option);
                    if word == "--" {
                        pos = Position::Command;
                    } else if precommand_flag_takes_arg(precmd, word) {
                        pos = Position::PrecommandFlagArg(precmd);
                    }
                } else if !has_quotes_or_var && is_reserved_word(word) {
                    push_span(spans, word_start, word_end, TokenKind::ReservedWord);
                    pos = Position::Argument;
                } else if has_quotes_or_var {
                    emit_word_subtokens(line, word_start, word_end, TokenKind::Default, spans);
                    pos = Position::Argument;
                } else {
                    let valid = resolver.command_exists(word);
                    let kind = if valid {
                        TokenKind::ValidCommand
                    } else {
                        TokenKind::InvalidCommand
                    };
                    push_span(spans, word_start, word_end, kind);
                    pos = if valid && is_precommand(word) {
                        Position::Precommand(static_precommand_name(word))
                    } else {
                        Position::Argument
                    };
                }
            }
            Position::PrecommandFlagArg(precmd) => {
                emit_word_subtokens(line, word_start, word_end, TokenKind::Default, spans);
                pos = Position::Precommand(precmd);
            }
            Position::RedirectTarget => {
                if has_quotes_or_var {
                    emit_word_subtokens(line, word_start, word_end, TokenKind::Default, spans);
                } else {
                    let kind = if path_checks < MAX_PATH_CHECKS_PER_LINE {
                        path_checks += 1;
                        if resolver.path_exists(word) {
                            TokenKind::ExistingPath
                        } else {
                            TokenKind::Default
                        }
                    } else {
                        TokenKind::Default
                    };
                    push_span(spans, word_start, word_end, kind);
                }
                pos = Position::Argument;
            }
            Position::Argument => {
                if has_quotes_or_var {
                    emit_word_subtokens(line, word_start, word_end, TokenKind::Default, spans);
                } else if word.starts_with('-') && word.len() > 1 {
                    if let Some(eq) = word.find('=') {
                        push_span(spans, word_start, word_start + eq, TokenKind::Option);
                        push_span(
                            spans,
                            word_start + eq,
                            word_start + eq + 1,
                            TokenKind::Operator,
                        );
                        let val_start = word_start + eq + 1;
                        if val_start < word_end {
                            let val = &line[val_start..word_end];
                            let val_kind = if path_checks < MAX_PATH_CHECKS_PER_LINE
                                && !val.is_empty()
                                && resolver.path_exists(val)
                            {
                                path_checks += 1;
                                TokenKind::ExistingPath
                            } else {
                                TokenKind::Default
                            };
                            push_span(spans, val_start, word_end, val_kind);
                        }
                    } else {
                        push_span(spans, word_start, word_end, TokenKind::Option);
                    }
                } else if path_checks < MAX_PATH_CHECKS_PER_LINE {
                    path_checks += 1;
                    let kind = if resolver.path_exists(word) {
                        TokenKind::ExistingPath
                    } else {
                        TokenKind::Default
                    };
                    push_span(spans, word_start, word_end, kind);
                } else {
                    push_span(spans, word_start, word_end, TokenKind::Default);
                }
            }
        }
    }
}

fn static_precommand_name(word: &str) -> &'static str {
    match word {
        "sudo" => "sudo",
        "doas" => "doas",
        "env" => "env",
        "command" => "command",
        "builtin" => "builtin",
        "exec" => "exec",
        "nohup" => "nohup",
        "nice" => "nice",
        "strace" => "strace",
        _ => "",
    }
}

fn push_span(spans: &mut Vec<Span>, start: usize, end: usize, kind: TokenKind) {
    if start >= end {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.end == start
        && last.kind == kind
    {
        last.end = end;
        return;
    }
    spans.push(Span { start, end, kind });
}

fn match_operator(bytes: &[u8], i: usize) -> Option<(usize, Position)> {
    let rest = &bytes[i..];
    if rest.starts_with(b"&&")
        || rest.starts_with(b"||")
        || rest.starts_with(b"|&")
        || rest.starts_with(b";;")
        || rest.starts_with(b";&")
    {
        return Some((2, Position::Command));
    }
    if rest.starts_with(b"&>") {
        // Handled by redirection matcher.
        return None;
    }
    match rest[0] {
        b'|' | b';' | b'&' | b'(' => Some((1, Position::Command)),
        b')' => Some((1, Position::Argument)),
        _ => None,
    }
}

fn match_redirection(bytes: &[u8], i: usize) -> Option<(usize, bool)> {
    let rest = &bytes[i..];
    if rest.starts_with(b"<<<") || rest.starts_with(b"&>>") {
        return Some((3, true));
    }
    if rest.starts_with(b">&") || rest.starts_with(b"<&") {
        // Consume optional target fd or `-` (e.g., `>&1`, `>&-`).
        let mut len = 2;
        if len < rest.len() && rest[len] == b'-' {
            len += 1;
            return Some((len, false));
        }
        let digit_start = len;
        while len < rest.len() && rest[len].is_ascii_digit() {
            len += 1;
        }
        if len > digit_start {
            return Some((len, false));
        }
        return Some((2, true));
    }
    if rest.starts_with(b">>")
        || rest.starts_with(b"<<")
        || rest.starts_with(b"&>")
        || rest.starts_with(b"<>")
        || rest.starts_with(b">|")
    {
        return Some((2, true));
    }
    if matches!(rest[0], b'>' | b'<') {
        return Some((1, true));
    }
    None
}

fn is_fd_dup(op: &[u8]) -> bool {
    if op.len() >= 3 && (op.starts_with(b">&") || op.starts_with(b"<&")) {
        return op[2] == b'-' || is_all_ascii_digits(&op[2..]);
    }
    false
}

/// Advance past a shell word (respecting single quotes, double quotes, and backslash escapes).
fn scan_word_end(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start;
    let mut in_single = false;
    let mut in_double = false;

    while i < len {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' if i + 1 < len => {
                i += 2;
            }
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b if b.is_ascii_whitespace()
                || matches!(b, b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>') =>
            {
                break;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Always advance at least 1 byte to guarantee progress.
    if i == start { start + 1 } else { i }
}

/// Emit sub-spans within a word containing quotes or `$VAR` expansions.
fn emit_word_subtokens(
    line: &str,
    start: usize,
    end: usize,
    base_kind: TokenKind,
    spans: &mut Vec<Span>,
) {
    let bytes = line.as_bytes();
    let mut i = start;

    while i < end {
        match bytes[i] {
            b'\'' => {
                let q_start = i;
                i += 1;
                while i < end && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < end {
                    i += 1;
                }
                push_span(spans, q_start, i, TokenKind::StringLiteral);
            }
            b'"' => {
                let mut seg_start = i;
                i += 1;
                while i < end {
                    if bytes[i] == b'\\' && i + 1 < end {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        push_span(spans, seg_start, i, TokenKind::StringLiteral);
                        seg_start = i;
                        break;
                    }
                    if bytes[i] == b'$'
                        && let Some(var_end) = scan_variable_end(bytes, i, end)
                    {
                        push_span(spans, seg_start, i, TokenKind::StringLiteral);
                        push_span(spans, i, var_end, TokenKind::Variable);
                        i = var_end;
                        seg_start = i;
                        continue;
                    }
                    i += 1;
                }
                if seg_start < i {
                    push_span(spans, seg_start, i, TokenKind::StringLiteral);
                }
            }
            b'$' => {
                if let Some(var_end) = scan_variable_end(bytes, i, end) {
                    push_span(spans, i, var_end, TokenKind::Variable);
                    i = var_end;
                } else {
                    push_span(spans, i, i + 1, base_kind);
                    i += 1;
                }
            }
            b'\\' if i + 1 < end => {
                push_span(spans, i, i + 2, base_kind);
                i += 2;
            }
            _ => {
                let seg_start = i;
                while i < end && !matches!(bytes[i], b'\'' | b'"' | b'$' | b'\\') {
                    i += 1;
                }
                push_span(spans, seg_start, i, base_kind);
            }
        }
    }
}

/// If `bytes[start]` is `$` starting a shell variable (`$FOO`, `${...}`, `$?`, `$1`, etc.),
/// return the exclusive end index; otherwise `None`.
fn scan_variable_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if start + 1 >= limit || bytes[start] != b'$' {
        return None;
    }
    let next = bytes[start + 1];
    if next == b'{' {
        let mut i = start + 2;
        let mut depth = 1usize;
        while i < limit && depth > 0 {
            match bytes[i] {
                b'\\' if i + 1 < limit => i += 2,
                b'{' => {
                    depth += 1;
                    i += 1;
                }
                b'}' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        return Some(i);
    }
    if next.is_ascii_alphabetic() || next == b'_' {
        let mut i = start + 2;
        while i < limit && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        return Some(i);
    }
    if next.is_ascii_digit() || matches!(next, b'?' | b'$' | b'!' | b'#' | b'*' | b'@' | b'-') {
        return Some(start + 2);
    }
    None
}

/// Cursor geometry for repainting `line` cleanly over Readline's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepaintPlan {
    /// Prompt + full line fits strictly on a single terminal row (`prompt_col + line_cols < screen_cols`).
    /// Move backward relative to Readline's cursor by `back_cols` (zero dependency on PS1 width!).
    SingleRow { back_cols: usize },
    /// Line wraps across multiple rows (`prompt_col + line_cols >= screen_cols`).
    /// Move up `cursor_row` rows and jump to 1-based column `prompt_col + 1`.
    Wrapped {
        cursor_row: usize,
        prompt_col: usize,
    },
}

/// Compute a safe repaint plan for overlaying syntax highlighting onto `line`.
/// Returns `None` if the line contains control characters (tabs/newlines/etc.) or
/// cannot be safely repainted.
pub fn compute_repaint_plan(
    prompt_bytes: &[u8],
    line: &str,
    point: usize,
    screen_cols: usize,
) -> Option<RepaintPlan> {
    if screen_cols <= 1 || line.is_empty() || point > line.len() || !line.is_char_boundary(point) {
        return None;
    }
    if line.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }

    let prompt_col = measure_prompt_last_line_col(prompt_bytes, screen_cols);

    let mut row = 0usize;
    let mut col = prompt_col;
    let mut back_cols = 0usize;
    let mut cursor_row = 0usize;
    let mut recorded_cursor = false;

    for (idx, ch) in line.char_indices() {
        if idx == point {
            cursor_row = row;
            recorded_cursor = true;
        }
        let w = char_display_width(ch);
        if idx < point {
            back_cols += w;
        }
        if w > 0 {
            if col + w > screen_cols {
                row += 1;
                col = w.min(screen_cols);
            } else {
                col += w;
                if col == screen_cols {
                    row += 1;
                    col = 0;
                }
            }
        }
    }

    if !recorded_cursor {
        cursor_row = row;
    }

    // Strict single row (`row == 0` and `col < screen_cols`): zero ambiguity with terminal wrap or PS1 width.
    if row == 0 && col < screen_cols {
        Some(RepaintPlan::SingleRow { back_cols })
    } else {
        Some(RepaintPlan::Wrapped {
            cursor_row,
            prompt_col,
        })
    }
}

/// Append ANSI-colored `line` (according to `spans`) into `out`, resetting SGR cleanly at the end.
pub fn render_highlighted_into(line: &str, spans: &[Span], out: &mut Vec<u8>) {
    let bytes = line.as_bytes();
    let mut active_sgr: Option<&'static [u8]> = None;

    for span in spans {
        let sgr = span.kind.sgr();
        if sgr != active_sgr {
            if active_sgr.is_some() {
                out.extend_from_slice(b"\x1b[0m");
            }
            if let Some(code) = sgr {
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(code);
                out.push(b'm');
            }
            active_sgr = sgr;
        }
        out.extend_from_slice(&bytes[span.start..span.end]);
    }

    if active_sgr.is_some() {
        out.extend_from_slice(b"\x1b[0m");
    }
}

/// Append CSI cursor-positioning sequence for `plan` into `out` (zero allocations).
pub fn emit_move_to_line_start(plan: RepaintPlan, out: &mut Vec<u8>) {
    match plan {
        RepaintPlan::SingleRow { back_cols } => {
            if back_cols > 0 {
                out.extend_from_slice(b"\x1b[");
                push_usize_ascii(back_cols, out);
                out.push(b'D');
            }
        }
        RepaintPlan::Wrapped {
            cursor_row,
            prompt_col,
        } => {
            if cursor_row > 0 {
                out.extend_from_slice(b"\x1b[");
                push_usize_ascii(cursor_row, out);
                out.push(b'A');
            }
            out.extend_from_slice(b"\x1b[");
            push_usize_ascii(prompt_col + 1, out);
            out.push(b'G');
        }
    }
}

fn push_usize_ascii(mut n: usize, out: &mut Vec<u8>) {
    let mut buf = [0u8; 20];
    let mut idx = buf.len();
    loop {
        idx -= 1;
        buf[idx] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[idx..]);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    struct MockResolver {
        cmds: HashSet<&'static str>,
        paths: HashSet<&'static str>,
    }

    impl MockResolver {
        fn new(cmds: &[&'static str], paths: &[&'static str]) -> Self {
            Self {
                cmds: cmds.iter().copied().collect(),
                paths: paths.iter().copied().collect(),
            }
        }
    }

    impl CommandResolver for MockResolver {
        fn command_exists(&mut self, word: &str) -> bool {
            self.cmds.contains(word)
        }
        fn path_exists(&mut self, word: &str) -> bool {
            self.paths.contains(word)
        }
    }

    fn lex<'a>(line: &'a str, r: &mut MockResolver) -> Vec<(&'a str, TokenKind)> {
        let mut spans = Vec::new();
        highlight_line(line, r, &mut spans);
        spans
            .into_iter()
            .map(|s| (&line[s.start..s.end], s.kind))
            .collect()
    }

    #[test]
    fn highlights_valid_and_invalid_commands_with_flags_and_paths() {
        let mut r = MockResolver::new(&["git", "ls"], &["Cargo.toml", "src/lib.rs"]);
        assert_eq!(
            lex("git status --short Cargo.toml", &mut r),
            vec![
                ("git", TokenKind::ValidCommand),
                (" status ", TokenKind::Default),
                ("--short", TokenKind::Option),
                (" ", TokenKind::Default),
                ("Cargo.toml", TokenKind::ExistingPath),
            ]
        );
        assert_eq!(
            lex("nonexistent_cmd foo", &mut r),
            vec![
                ("nonexistent_cmd", TokenKind::InvalidCommand),
                (" foo", TokenKind::Default),
            ]
        );
    }

    #[test]
    fn highlights_pipes_redirections_assignments_and_precommands() {
        let mut r = MockResolver::new(&["sudo", "env", "cargo", "grep"], &["out.log"]);
        assert_eq!(
            lex(
                "RUST_LOG=debug sudo -u root cargo test | grep -i fail 2>&1 > out.log",
                &mut r
            ),
            vec![
                ("RUST_LOG", TokenKind::Variable),
                ("=", TokenKind::Operator),
                ("debug ", TokenKind::Default),
                ("sudo", TokenKind::ValidCommand),
                (" ", TokenKind::Default),
                ("-u", TokenKind::Option),
                (" root ", TokenKind::Default),
                ("cargo", TokenKind::ValidCommand),
                (" test ", TokenKind::Default),
                ("|", TokenKind::Operator),
                (" ", TokenKind::Default),
                ("grep", TokenKind::ValidCommand),
                (" ", TokenKind::Default),
                ("-i", TokenKind::Option),
                (" fail ", TokenKind::Default),
                ("2>&1", TokenKind::Operator),
                (" ", TokenKind::Default),
                (">", TokenKind::Operator),
                (" ", TokenKind::Default),
                ("out.log", TokenKind::ExistingPath),
            ]
        );
    }

    #[test]
    fn highlights_strings_variables_comments_and_reserved_words() {
        let mut r = MockResolver::new(&["echo"], &[]);
        assert_eq!(
            lex("if echo \"hi $USER\" 'literal $NO' # note", &mut r),
            vec![
                ("if", TokenKind::ReservedWord),
                (" ", TokenKind::Default),
                ("echo", TokenKind::ValidCommand),
                (" ", TokenKind::Default),
                ("\"hi ", TokenKind::StringLiteral),
                ("$USER", TokenKind::Variable),
                ("\"", TokenKind::StringLiteral),
                (" ", TokenKind::Default),
                ("'literal $NO'", TokenKind::StringLiteral),
                (" ", TokenKind::Default),
                ("# note", TokenKind::Comment),
            ]
        );
    }

    #[test]
    fn highlights_flag_equals_path_and_brace_vars() {
        let mut r = MockResolver::new(&["rg"], &["src"]);
        assert_eq!(
            lex("rg --path=src ${HOME}/bin", &mut r),
            vec![
                ("rg", TokenKind::ValidCommand),
                (" ", TokenKind::Default),
                ("--path", TokenKind::Option),
                ("=", TokenKind::Operator),
                ("src", TokenKind::ExistingPath),
                (" ", TokenKind::Default),
                ("${HOME}", TokenKind::Variable),
                ("/bin", TokenKind::Default),
            ]
        );
    }

    #[test]
    fn computes_single_row_and_wrapped_repaint_plans() {
        // "$ " (2) + "git status" (10) = 12 < 80 -> single row, cursor at 4 -> back 4
        assert_eq!(
            compute_repaint_plan(b"$ ", "git status", 4, 80),
            Some(RepaintPlan::SingleRow { back_cols: 4 })
        );

        // CJK width before cursor: "echo 你好" -> "echo " (5) + "你" (2) = 7
        let line = "echo 你好";
        let point = "echo 你".len();
        assert_eq!(
            compute_repaint_plan(b"$ ", line, point, 80),
            Some(RepaintPlan::SingleRow { back_cols: 7 })
        );

        // Wrapped line: "$ " (2) + 12 chars in 10-col terminal -> wraps to row 1
        assert_eq!(
            compute_repaint_plan(b"$ ", "123456789012", 12, 10),
            Some(RepaintPlan::Wrapped {
                cursor_row: 1,
                prompt_col: 2,
            })
        );

        // Control chars / newlines safely return None.
        assert_eq!(compute_repaint_plan(b"$ ", "echo\nhi", 4, 80), None);
        assert_eq!(compute_repaint_plan(b"$ ", "echo\thi", 4, 80), None);
    }

    #[test]
    fn renders_ansi_sequences_cleanly() {
        let mut r = MockResolver::new(&["echo"], &[]);
        let mut spans = Vec::new();
        let line = "echo 'hi'";
        highlight_line(line, &mut r, &mut spans);
        let mut out = Vec::new();
        render_highlighted_into(line, &spans, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[32mecho\x1b[0m \x1b[33m'hi'\x1b[0m"
        );
    }

    #[test]
    fn parses_bool_env_values() {
        assert!(parse_bool_env("1"));
        assert!(parse_bool_env("on"));
        assert!(parse_bool_env("true"));
        assert!(!parse_bool_env("0"));
        assert!(!parse_bool_env("off"));
        assert!(!parse_bool_env("FALSE"));
    }
}
