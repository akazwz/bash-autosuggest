//! Readline hook installation, zero-allocation ghost-text rendering, and keybinding wrappers.
//!
//! All Bash / Readline callbacks execute strictly on Bash's main thread, so state lives
//! in a single-thread `RefCell` with pre-allocated reusable buffers (zero heap allocation
//! on the per-keystroke hot path).

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};

use crate::ffi::{
    _rl_last_c_pos, ISFUNC, Keymap, RL_STATE_SUPPRESS_MASK, RlCommandFunc, RlHookFunc, RlVoidFunc,
    emacs_standard_keymap, fflush, fwrite, history_length, history_list, rl_bind_keyseq_in_map,
    rl_done, rl_end, rl_end_of_line, rl_forward_char, rl_forward_word, rl_function_of_keyseq_len,
    rl_get_screen_size, rl_insert_text, rl_line_buffer, rl_newline, rl_outstream, rl_point,
    rl_pre_input_hook, rl_readline_state, rl_redisplay, rl_redisplay_function, vi_insertion_keymap,
};
use crate::suggest::{
    find_suggestion_bytes, is_supported_term, next_word_prefix, sanitize_sgr_style,
    truncate_to_columns,
};

const DEFAULT_SGR_STYLE: &str = "38;5;242";
const ERASE_TO_EOL: &[u8] = b"\x1b[K";

/// Key sequence + original binding snapshot for clean uninstall.
struct SavedBinding {
    keymap: Keymap,
    seq: &'static [u8],
    original: Option<RlCommandFunc>,
}

struct HookState {
    enabled: bool,
    ghost_drawn: bool,
    suggestion: String,
    out_buf: Vec<u8>,
    style: String,
    orig_redisplay: Option<RlVoidFunc>,
    orig_pre_input: Option<RlHookFunc>,
    orig_forward_char: Option<RlCommandFunc>,
    orig_end_of_line: Option<RlCommandFunc>,
    orig_forward_word: Option<RlCommandFunc>,
    orig_newline: Option<RlCommandFunc>,
    orig_tab: Option<RlCommandFunc>,
    saved_bindings: Vec<SavedBinding>,
}

impl HookState {
    const fn new() -> Self {
        Self {
            enabled: false,
            ghost_drawn: false,
            suggestion: String::new(),
            out_buf: Vec::new(),
            style: String::new(),
            orig_redisplay: None,
            orig_pre_input: None,
            orig_forward_char: None,
            orig_end_of_line: None,
            orig_forward_word: None,
            orig_newline: None,
            orig_tab: None,
            saved_bindings: Vec::new(),
        }
    }

    fn style_str(&self) -> &str {
        if self.style.is_empty() {
            DEFAULT_SGR_STYLE
        } else {
            &self.style
        }
    }
}

thread_local! {
    static STATE: RefCell<HookState> = const { RefCell::new(HookState::new()) };
}

fn load_style_from_env() -> String {
    if let Ok(val) = std::env::var("BASH_AUTOSUGGEST_STYLE")
        && let Some(clean) = sanitize_sgr_style(&val)
    {
        return clean.to_owned();
    }
    DEFAULT_SGR_STYLE.to_owned()
}

/// Write raw bytes to Readline's `rl_outstream` and flush immediately.
///
/// # Safety
/// Must be called from Bash's main thread while Readline is initialized.
unsafe fn write_rl_out(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    // SAFETY: `rl_outstream` is managed by GNU Readline on Bash's main thread; we guard
    // against null before passing it to C `fwrite`/`fflush`.
    unsafe {
        let stream = rl_outstream;
        if !stream.is_null() {
            let _ = fwrite(bytes.as_ptr().cast::<c_void>(), 1, bytes.len(), stream);
            let _ = fflush(stream);
        }
    }
}

/// Clear any ghost text currently displayed at/after the cursor.
///
/// # Safety
/// Must be called from Bash's main thread while Readline is initialized.
unsafe fn clear_ghost_if_drawn() {
    let need_clear = STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        if st.ghost_drawn {
            st.ghost_drawn = false;
            true
        } else {
            false
        }
    });
    if need_clear {
        // SAFETY: Caller guarantees Readline main-thread context.
        unsafe { write_rl_out(ERASE_TO_EOL) };
    }
}

/// Search Bash's in-memory Readline history table and write the matching suffix into `dest`
/// without heap allocation (reuses `dest`'s existing capacity).
///
/// # Safety
/// Must be called from Bash's main thread while Readline history is initialized.
unsafe fn search_bash_history_into(prefix: &str, dest: &mut String) -> bool {
    dest.clear();
    // SAFETY: `history_length` and `history_list()` are owned by GNU Readline on Bash's
    // single main thread and remain valid for the duration of this synchronous callback.
    unsafe {
        let len = history_length;
        if len <= 0 {
            return false;
        }
        let list = history_list();
        if list.is_null() {
            return false;
        }

        let slice = std::slice::from_raw_parts(list, len as usize);
        let entries = slice.iter().rev().filter_map(|&entry_ptr| {
            if entry_ptr.is_null() {
                return None;
            }
            let line_ptr = (*entry_ptr).line;
            if line_ptr.is_null() {
                return None;
            }
            Some(CStr::from_ptr(line_ptr).to_bytes())
        });

        if let Some(suffix) = find_suggestion_bytes(prefix, entries) {
            dest.push_str(suffix);
            true
        } else {
            false
        }
    }
}

/// Read Readline's current line buffer (`0..rl_end`) as UTF-8.
///
/// # Safety
/// Returned slice borrows Readline's `rl_line_buffer` and must not outlive the current
/// synchronous Readline callback.
unsafe fn current_line_str() -> Option<&'static str> {
    // SAFETY: `rl_line_buffer` points to a valid buffer of at least `rl_end` bytes during
    // any Readline redisplay or keybinding callback.
    unsafe {
        let buf = rl_line_buffer;
        let end = rl_end;
        if buf.is_null() || end <= 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(buf.cast::<u8>(), end as usize);
        std::str::from_utf8(bytes).ok()
    }
}

/// Check whether Readline is in normal end-of-line editing state suitable for ghost text.
///
/// # Safety
/// Must be called from Bash's main thread.
unsafe fn should_suggest() -> bool {
    // SAFETY: Reading Readline globals on Bash's main thread.
    unsafe {
        if rl_done != 0 {
            return false;
        }
        if (rl_readline_state & RL_STATE_SUPPRESS_MASK) != 0 {
            return false;
        }
        rl_end > 0 && rl_point == rl_end
    }
}

/// Wrapped `rl_redisplay_function`:
/// 1. Clears previous ghost text before Readline paints (prevents residue on `←`,
///    Backspace, `Ctrl+R`, or non-matching keystrokes).
/// 2. Calls the original `rl_redisplay`.
/// 3. If cursor is at end-of-line in normal editing state, finds the newest history
///    suffix and draws it in grey using reusable buffers (0 heap allocations).
unsafe extern "C" fn custom_redisplay() {
    // SAFETY: Invoked directly by GNU Readline on Bash's main thread.
    unsafe {
        clear_ghost_if_drawn();

        let orig = STATE.with(|cell| cell.borrow().orig_redisplay);
        if let Some(f) = orig {
            f();
        } else {
            rl_redisplay();
        }

        if !should_suggest() {
            STATE.with(|cell| cell.borrow_mut().suggestion.clear());
            return;
        }

        let Some(prefix) = current_line_str() else {
            STATE.with(|cell| cell.borrow_mut().suggestion.clear());
            return;
        };

        let mut rows: c_int = 0;
        let mut cols: c_int = 0;
        rl_get_screen_size(&mut rows, &mut cols);
        let cur_col = _rl_last_c_pos.max(0);
        // Leave 1 column margin so the terminal never triggers auto-wrap at the right edge.
        let avail_cols = if cols > cur_col + 1 {
            (cols - cur_col - 1) as usize
        } else {
            0
        };

        STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            if !search_bash_history_into(prefix, &mut st.suggestion) {
                st.ghost_drawn = false;
                return;
            }

            let display_len = truncate_to_columns(&st.suggestion, avail_cols).len();
            if display_len == 0 {
                st.ghost_drawn = false;
                return;
            }

            let HookState {
                suggestion,
                out_buf,
                style,
                ghost_drawn,
                ..
            } = &mut *st;
            let style_bytes = if style.is_empty() {
                DEFAULT_SGR_STYLE.as_bytes()
            } else {
                style.as_bytes()
            };

            out_buf.clear();
            out_buf.extend_from_slice(b"\x1b[K\x1b7\x1b[");
            out_buf.extend_from_slice(style_bytes);
            out_buf.push(b'm');
            out_buf.extend_from_slice(&suggestion.as_bytes()[..display_len]);
            out_buf.extend_from_slice(b"\x1b[0m\x1b8");
            *ghost_drawn = true;

            write_rl_out(out_buf);
        });
    }
}

/// Reset per-prompt state cleanly at the start of every new Readline input cycle.
unsafe extern "C" fn custom_pre_input_hook() -> c_int {
    let orig = STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        st.ghost_drawn = false;
        st.suggestion.clear();
        st.orig_pre_input
    });

    if let Some(f) = orig {
        // SAFETY: `orig` was captured from Readline's `rl_pre_input_hook`.
        unsafe { f() }
    } else {
        0
    }
}

/// Insert full suggestion into Readline if at end-of-line with an active suggestion.
/// Returns `true` if a suggestion was accepted.
unsafe fn try_accept_full() -> bool {
    // SAFETY: Called from Readline keybinding callbacks on Bash's main thread.
    unsafe {
        if !should_suggest() {
            return false;
        }
        let text_opt = STATE.with(|cell| {
            let mut st = cell.borrow_mut();
            if st.suggestion.is_empty() {
                None
            } else {
                let cstr = CString::new(st.suggestion.as_str()).ok();
                st.suggestion.clear();
                cstr
            }
        });
        if let Some(cstr) = text_opt {
            clear_ghost_if_drawn();
            rl_insert_text(cstr.as_ptr());
            return true;
        }
        false
    }
}

/// Accept full suggestion when at end-of-line (`→` / `Ctrl+F`), otherwise forward-char.
unsafe extern "C" fn accept_or_forward_char(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if try_accept_full() {
            return 0;
        }
        let orig = STATE.with(|cell| cell.borrow().orig_forward_char);
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_forward_char(count, key)
        }
    }
}

/// Accept full suggestion when at end-of-line (`End`), otherwise move to end-of-line.
unsafe extern "C" fn accept_or_end_of_line(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if try_accept_full() {
            return 0;
        }
        let orig = STATE.with(|cell| cell.borrow().orig_end_of_line);
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_end_of_line(count, key)
        }
    }
}

/// Accept one word of the suggestion when at end-of-line (`Alt+→` / `Alt+f`), otherwise forward-word.
unsafe extern "C" fn accept_word_or_forward_word(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if should_suggest() {
            let chunk_opt = STATE.with(|cell| {
                let st = cell.borrow();
                let word = next_word_prefix(&st.suggestion);
                if word.is_empty() {
                    None
                } else {
                    CString::new(word).ok()
                }
            });
            if let Some(cstr) = chunk_opt {
                clear_ghost_if_drawn();
                rl_insert_text(cstr.as_ptr());
                return 0;
            }
        }

        let orig = STATE.with(|cell| cell.borrow().orig_forward_word);
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_forward_word(count, key)
        }
    }
}

/// Erase ghost text before executing the command on Enter so the executed line stays clean.
unsafe extern "C" fn clear_and_newline(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        clear_ghost_if_drawn();
        STATE.with(|cell| cell.borrow_mut().suggestion.clear());

        let orig = STATE.with(|cell| cell.borrow().orig_newline);
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_newline(count, key)
        }
    }
}

/// Erase ghost text before running Tab completion so candidate lists never leave residue.
unsafe extern "C" fn clear_and_tab(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        clear_ghost_if_drawn();
        STATE.with(|cell| cell.borrow_mut().suggestion.clear());

        let orig = STATE.with(|cell| cell.borrow().orig_tab);
        if let Some(f) = orig { f(count, key) } else { 0 }
    }
}

unsafe fn lookup_binding(keymap: Keymap, seq: &[u8]) -> Option<RlCommandFunc> {
    // SAFETY: `seq` is NUL-terminated static bytes; `keymap` is a valid Readline keymap.
    unsafe {
        let mut kind: c_int = -1;
        let len = seq.len().saturating_sub(1);
        let func = rl_function_of_keyseq_len(seq.as_ptr().cast::<c_char>(), len, keymap, &mut kind);
        if kind == ISFUNC { func } else { None }
    }
}

unsafe fn bind_and_record(
    st: &mut HookState,
    keymap: Keymap,
    seq: &'static [u8],
    handler: RlCommandFunc,
) {
    // SAFETY: `seq` is NUL-terminated static bytes; `keymap` is a valid Readline keymap.
    unsafe {
        let original = lookup_binding(keymap, seq);
        st.saved_bindings.push(SavedBinding {
            keymap,
            seq,
            original,
        });
        rl_bind_keyseq_in_map(seq.as_ptr().cast::<c_char>(), Some(handler), keymap);
    }
}

/// Enable autosuggest hooks and keybindings (idempotent; no-op on `TERM=dumb`).
pub fn enable() {
    let term = std::env::var("TERM").ok();
    if !is_supported_term(term.as_deref()) {
        return;
    }

    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        st.style = load_style_from_env();
        if st.enabled {
            return;
        }

        // Pre-allocate reusable buffers once to keep per-keystroke redisplay allocation-free.
        if st.suggestion.capacity() < 256 {
            st.suggestion.reserve(256);
        }
        if st.out_buf.capacity() < 320 {
            st.out_buf.reserve(320);
        }

        // SAFETY: Called from Bash's builtin dispatcher on Bash's main thread.
        unsafe {
            let orig_r = match rl_redisplay_function {
                Some(f) if std::ptr::fn_addr_eq(f, custom_redisplay as RlVoidFunc) => {
                    Some(rl_redisplay as RlVoidFunc)
                }
                other => other.or(Some(rl_redisplay as RlVoidFunc)),
            };
            st.orig_redisplay = orig_r;
            rl_redisplay_function = Some(custom_redisplay);

            st.orig_pre_input = match rl_pre_input_hook {
                Some(f) if std::ptr::fn_addr_eq(f, custom_pre_input_hook as RlHookFunc) => None,
                other => other,
            };
            rl_pre_input_hook = Some(custom_pre_input_hook);

            let emacs_map: Keymap = &raw mut emacs_standard_keymap;
            let vi_map: Keymap = &raw mut vi_insertion_keymap;

            st.orig_forward_char = lookup_binding(emacs_map, b"\x1b[C\0").or(Some(rl_forward_char));
            st.orig_end_of_line = lookup_binding(emacs_map, b"\x1b[F\0").or(Some(rl_end_of_line));
            st.orig_forward_word =
                lookup_binding(emacs_map, b"\x1b[1;3C\0").or(Some(rl_forward_word));
            st.orig_newline = lookup_binding(emacs_map, b"\r\0").or(Some(rl_newline));
            st.orig_tab = lookup_binding(emacs_map, b"\t\0");

            for &map in &[emacs_map, vi_map] {
                bind_and_record(&mut st, map, b"\x1b[C\0", accept_or_forward_char);
                bind_and_record(&mut st, map, b"\x1bOC\0", accept_or_forward_char);
                bind_and_record(&mut st, map, b"\x1b[F\0", accept_or_end_of_line);
                bind_and_record(&mut st, map, b"\x1bOF\0", accept_or_end_of_line);
                bind_and_record(&mut st, map, b"\x1b[1;3C\0", accept_word_or_forward_word);
                bind_and_record(&mut st, map, b"\x1b[3C\0", accept_word_or_forward_word);
                bind_and_record(&mut st, map, b"\x1b\x1b[C\0", accept_word_or_forward_word);
                bind_and_record(&mut st, map, b"\r\0", clear_and_newline);
                bind_and_record(&mut st, map, b"\n\0", clear_and_newline);
                if st.orig_tab.is_some() {
                    bind_and_record(&mut st, map, b"\t\0", clear_and_tab);
                }
            }

            // Standard emacs full/word accept shortcuts (`Ctrl+F` and `Alt+f`).
            bind_and_record(&mut st, emacs_map, b"\x06\0", accept_or_forward_char);
            bind_and_record(&mut st, emacs_map, b"\x1bf\0", accept_word_or_forward_word);
        }

        st.enabled = true;
    });
}

/// Disable autosuggest hooks and restore all Readline callbacks and keybindings.
pub fn disable() {
    // SAFETY: Called from Bash's builtin dispatcher or unload hook on Bash's main thread.
    unsafe { clear_ghost_if_drawn() };

    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        if !st.enabled {
            return;
        }

        // SAFETY: Restoring original Readline function pointers and keymap entries on main thread.
        unsafe {
            rl_redisplay_function = st.orig_redisplay.take().or(Some(rl_redisplay));
            rl_pre_input_hook = st.orig_pre_input.take();

            for saved in st.saved_bindings.drain(..).rev() {
                rl_bind_keyseq_in_map(
                    saved.seq.as_ptr().cast::<c_char>(),
                    saved.original,
                    saved.keymap,
                );
            }
        }

        st.suggestion.clear();
        st.ghost_drawn = false;
        st.enabled = false;
    });
}

/// Return a human-readable status summary.
pub fn status() -> String {
    let (enabled, style) = STATE.with(|cell| {
        let st = cell.borrow();
        (st.enabled, st.style_str().to_owned())
    });
    // SAFETY: Reading `history_length` on Bash's main thread.
    let hist_len = unsafe { history_length };
    format!(
        "bash_autosuggest: {} (style=\"{}\", history_entries={})",
        if enabled { "enabled" } else { "disabled" },
        style,
        hist_len
    )
}
