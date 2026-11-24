//! Readline hook installation, zero-allocation ghost-text rendering, and keybinding wrappers.
//!
//! All Bash / Readline callbacks execute strictly on Bash's main thread, so state lives
//! in a single-thread `RefCell` with pre-allocated reusable buffers (zero heap allocation
//! on the per-keystroke hot path). All callbacks use fallible `try_borrow` / `try_borrow_mut`
//! so unexpected signal-handler re-entry can never panic or abort the host Bash process.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};

use crate::ffi::{
    ISFUNC, Keymap, RL_STATE_SUPPRESS_MASK, RlCommandFunc, RlHookFunc, RlVoidFunc,
    emacs_standard_keymap, fflush, fwrite, history_length, history_list, rl_bind_keyseq_in_map,
    rl_display_prompt, rl_done, rl_end, rl_end_of_line, rl_forward_char, rl_forward_word,
    rl_function_of_keyseq_len, rl_get_keymap, rl_get_screen_size, rl_insert_text, rl_line_buffer,
    rl_newline, rl_outstream, rl_point, rl_pre_input_hook, rl_prompt, rl_readline_state,
    rl_redisplay, rl_redisplay_function, vi_insertion_keymap, vi_movement_keymap,
};
use crate::suggest::{
    find_suggestion_bytes, is_supported_term, next_word_prefix, remaining_line_columns,
    sanitize_sgr_style, truncate_to_columns,
};

const DEFAULT_SGR_STYLE: &str = "38;5;242";
const ERASE_TO_EOL: &[u8] = b"\x1b[K";

/// Key sequence + original binding snapshot for clean uninstall.
struct SavedBinding {
    keymap: Keymap,
    seq: &'static [u8],
    original: Option<RlCommandFunc>,
}

/// Original `ISFUNC` bindings captured per keymap so fallback calls preserve user config.
#[derive(Clone, Copy)]
struct KeymapFallbacks {
    right_arrow: Option<RlCommandFunc>,
    ctrl_f: Option<RlCommandFunc>,
    end_of_line: Option<RlCommandFunc>,
    alt_right: Option<RlCommandFunc>,
    alt_f: Option<RlCommandFunc>,
    newline: Option<RlCommandFunc>,
    tab: Option<RlCommandFunc>,
}

impl KeymapFallbacks {
    const fn empty() -> Self {
        Self {
            right_arrow: None,
            ctrl_f: None,
            end_of_line: None,
            alt_right: None,
            alt_f: None,
            newline: None,
            tab: None,
        }
    }
}

enum BindingLookup {
    Unbound,
    Func(RlCommandFunc),
    /// Existing macro (`ISMACR`) or sub-keymap (`ISKMAP`) that we must not clobber.
    Other,
}

impl BindingLookup {
    fn as_func(&self) -> Option<RlCommandFunc> {
        match *self {
            Self::Func(f) => Some(f),
            Self::Unbound | Self::Other => None,
        }
    }
}

struct HookState {
    enabled: bool,
    ghost_drawn: bool,
    suggestion: String,
    out_buf: Vec<u8>,
    style: String,
    orig_redisplay: Option<RlVoidFunc>,
    orig_pre_input: Option<RlHookFunc>,
    emacs_fallbacks: KeymapFallbacks,
    vi_fallbacks: KeymapFallbacks,
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
            emacs_fallbacks: KeymapFallbacks::empty(),
            vi_fallbacks: KeymapFallbacks::empty(),
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

    fn fallbacks_for(&self, active_map: Keymap, vi_ins_map: Keymap) -> KeymapFallbacks {
        if active_map == vi_ins_map {
            self.vi_fallbacks
        } else {
            self.emacs_fallbacks
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
        let Ok(mut st) = cell.try_borrow_mut() else {
            return false;
        };
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

/// Borrow Readline's current line buffer (`0..rl_end`) as UTF-8 within a synchronous callback.
///
/// # Safety
/// Must be called from Bash's main thread during a Readline redisplay or keybinding callback.
unsafe fn with_current_line<R>(f: impl FnOnce(&str) -> R) -> Option<R> {
    // SAFETY: `rl_line_buffer` points to a valid buffer of at least `rl_end` bytes during
    // any synchronous Readline callback and is not mutated while `f` runs.
    let line = unsafe {
        let buf = rl_line_buffer;
        let end = rl_end;
        if buf.is_null() || end <= 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(buf.cast::<u8>(), end as usize);
        std::str::from_utf8(bytes).ok()?
    };
    Some(f(line))
}

/// Borrow Readline's current display prompt bytes (`rl_display_prompt`, falling back to `rl_prompt`).
///
/// # Safety
/// Must be called from Bash's main thread during a synchronous Readline callback.
unsafe fn with_display_prompt_bytes<R>(f: impl FnOnce(&[u8]) -> R) -> R {
    // SAFETY: `rl_display_prompt` / `rl_prompt` are NUL-terminated C strings owned by
    // GNU Readline on Bash's main thread for the duration of this synchronous callback.
    let bytes = unsafe {
        let ptr = if !rl_display_prompt.is_null() {
            rl_display_prompt
        } else {
            rl_prompt
        };
        if ptr.is_null() {
            &[]
        } else {
            CStr::from_ptr(ptr).to_bytes()
        }
    };
    f(bytes)
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
        let vi_cmd_map: Keymap = &raw mut vi_movement_keymap;
        if rl_get_keymap() == vi_cmd_map {
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

        let orig = STATE.with(|cell| cell.try_borrow().ok().and_then(|st| st.orig_redisplay));
        if let Some(f) = orig {
            f();
        } else {
            rl_redisplay();
        }

        if !should_suggest() {
            STATE.with(|cell| {
                if let Ok(mut st) = cell.try_borrow_mut() {
                    st.suggestion.clear();
                }
            });
            return;
        }

        let mut rows: c_int = 0;
        let mut cols: c_int = 0;
        rl_get_screen_size(&mut rows, &mut cols);
        let screen_cols = cols.max(0) as usize;

        let drawn = with_current_line(|prefix| {
            let avail_cols = with_display_prompt_bytes(|prompt_bytes| {
                remaining_line_columns(prompt_bytes, prefix, screen_cols)
            });

            STATE.with(|cell| {
                let Ok(mut st) = cell.try_borrow_mut() else {
                    return;
                };
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
        });

        if drawn.is_none() {
            STATE.with(|cell| {
                if let Ok(mut st) = cell.try_borrow_mut() {
                    st.suggestion.clear();
                }
            });
        }
    }
}

/// Reset per-prompt state cleanly at the start of every new Readline input cycle.
unsafe extern "C" fn custom_pre_input_hook() -> c_int {
    let orig = STATE.with(|cell| {
        let Ok(mut st) = cell.try_borrow_mut() else {
            return None;
        };
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
            let Ok(mut st) = cell.try_borrow_mut() else {
                return None;
            };
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

/// Insert the next word of the active suggestion and immediately advance `st.suggestion`
/// so rapid repeated word accepts never duplicate text even if Readline defers redisplay.
unsafe fn try_accept_word() -> bool {
    // SAFETY: Called from Readline keybinding callbacks on Bash's main thread.
    unsafe {
        if !should_suggest() {
            return false;
        }
        let chunk_opt = STATE.with(|cell| {
            let Ok(mut st) = cell.try_borrow_mut() else {
                return None;
            };
            let word_len = next_word_prefix(&st.suggestion).len();
            if word_len == 0 {
                return None;
            }
            let cstr = CString::new(&st.suggestion[..word_len]).ok();
            if cstr.is_some() {
                st.suggestion.drain(..word_len);
            }
            cstr
        });
        if let Some(cstr) = chunk_opt {
            clear_ghost_if_drawn();
            rl_insert_text(cstr.as_ptr());
            return true;
        }
        false
    }
}

unsafe fn current_fallbacks() -> KeymapFallbacks {
    // SAFETY: Querying Readline keymap globals on Bash's main thread.
    let (active_map, vi_ins_map) = unsafe { (rl_get_keymap(), &raw mut vi_insertion_keymap) };
    STATE.with(|cell| {
        cell.try_borrow()
            .map(|st| st.fallbacks_for(active_map, vi_ins_map))
            .unwrap_or_else(|_| KeymapFallbacks::empty())
    })
}

/// Accept full suggestion when at end-of-line (`→`), otherwise original Right Arrow binding.
unsafe extern "C" fn accept_or_right_arrow(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if try_accept_full() {
            return 0;
        }
        let orig = current_fallbacks().right_arrow;
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_forward_char(count, key)
        }
    }
}

/// Accept full suggestion when at end-of-line (`Ctrl+F`), otherwise original `Ctrl+F` binding.
unsafe extern "C" fn accept_or_ctrl_f(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if try_accept_full() {
            return 0;
        }
        let orig = STATE.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|st| st.emacs_fallbacks.ctrl_f)
        });
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
        let orig = current_fallbacks().end_of_line;
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_end_of_line(count, key)
        }
    }
}

/// Accept one word of the suggestion when at end-of-line (`Alt+→`), otherwise forward-word.
unsafe extern "C" fn accept_word_or_alt_right(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if try_accept_word() {
            return 0;
        }
        let orig = current_fallbacks().alt_right;
        if let Some(f) = orig {
            f(count, key)
        } else {
            rl_forward_word(count, key)
        }
    }
}

/// Accept one word of the suggestion when at end-of-line (`Alt+f`), otherwise original `Alt+f`.
unsafe extern "C" fn accept_word_or_alt_f(count: c_int, key: c_int) -> c_int {
    // SAFETY: Invoked by GNU Readline on Bash's main thread.
    unsafe {
        if try_accept_word() {
            return 0;
        }
        let orig = STATE.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|st| st.emacs_fallbacks.alt_f)
        });
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
        STATE.with(|cell| {
            if let Ok(mut st) = cell.try_borrow_mut() {
                st.suggestion.clear();
            }
        });

        let orig = current_fallbacks().newline;
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
        STATE.with(|cell| {
            if let Ok(mut st) = cell.try_borrow_mut() {
                st.suggestion.clear();
            }
        });

        let orig = current_fallbacks().tab;
        if let Some(f) = orig { f(count, key) } else { 0 }
    }
}

unsafe fn lookup_binding(keymap: Keymap, seq: &[u8]) -> BindingLookup {
    // SAFETY: `seq` is NUL-terminated static bytes; `keymap` is a valid Readline keymap.
    unsafe {
        let mut kind: c_int = -1;
        let len = seq.len().saturating_sub(1);
        let func = rl_function_of_keyseq_len(seq.as_ptr().cast::<c_char>(), len, keymap, &mut kind);
        match func {
            None => BindingLookup::Unbound,
            Some(f) if kind == ISFUNC => BindingLookup::Func(f),
            Some(_) => BindingLookup::Other,
        }
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
        let original = match lookup_binding(keymap, seq) {
            BindingLookup::Unbound => None,
            BindingLookup::Func(f) => Some(f),
            // Never overwrite a user's custom macro (`ISMACR`) or prefix sub-keymap (`ISKMAP`).
            BindingLookup::Other => return,
        };
        st.saved_bindings.push(SavedBinding {
            keymap,
            seq,
            original,
        });
        rl_bind_keyseq_in_map(seq.as_ptr().cast::<c_char>(), Some(handler), keymap);
    }
}

unsafe fn capture_fallbacks(keymap: Keymap) -> KeymapFallbacks {
    // SAFETY: `keymap` is a valid Readline keymap on Bash's main thread.
    unsafe {
        KeymapFallbacks {
            right_arrow: lookup_binding(keymap, b"\x1b[C\0")
                .as_func()
                .or(Some(rl_forward_char)),
            ctrl_f: lookup_binding(keymap, b"\x06\0")
                .as_func()
                .or(Some(rl_forward_char)),
            end_of_line: lookup_binding(keymap, b"\x1b[F\0")
                .as_func()
                .or(Some(rl_end_of_line)),
            alt_right: lookup_binding(keymap, b"\x1b[1;3C\0")
                .as_func()
                .or(Some(rl_forward_word)),
            alt_f: lookup_binding(keymap, b"\x1bf\0")
                .as_func()
                .or(Some(rl_forward_word)),
            newline: lookup_binding(keymap, b"\r\0")
                .as_func()
                .or(Some(rl_newline)),
            tab: lookup_binding(keymap, b"\t\0").as_func(),
        }
    }
}

/// Enable autosuggest hooks and keybindings (idempotent; no-op on `TERM=dumb`).
pub fn enable() {
    let term = std::env::var("TERM").ok();
    if !is_supported_term(term.as_deref()) {
        return;
    }

    STATE.with(|cell| {
        let Ok(mut st) = cell.try_borrow_mut() else {
            return;
        };
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

            st.emacs_fallbacks = capture_fallbacks(emacs_map);
            st.vi_fallbacks = capture_fallbacks(vi_map);

            let emacs_has_tab = st.emacs_fallbacks.tab.is_some();
            let vi_has_tab = st.vi_fallbacks.tab.is_some();

            // Shared arrow / End / Enter / Tab bindings for both emacs and vi-insert keymaps.
            for (map, has_tab) in [(emacs_map, emacs_has_tab), (vi_map, vi_has_tab)] {
                bind_and_record(&mut st, map, b"\x1b[C\0", accept_or_right_arrow);
                bind_and_record(&mut st, map, b"\x1bOC\0", accept_or_right_arrow);
                bind_and_record(&mut st, map, b"\x1b[F\0", accept_or_end_of_line);
                bind_and_record(&mut st, map, b"\x1bOF\0", accept_or_end_of_line);
                bind_and_record(&mut st, map, b"\x1b[1;3C\0", accept_word_or_alt_right);
                bind_and_record(&mut st, map, b"\x1b[3C\0", accept_word_or_alt_right);
                bind_and_record(&mut st, map, b"\r\0", clear_and_newline);
                bind_and_record(&mut st, map, b"\n\0", clear_and_newline);
                if has_tab {
                    bind_and_record(&mut st, map, b"\t\0", clear_and_tab);
                }
            }

            // Emacs-only shortcuts (`Ctrl+F`, `Alt+f`, and double-ESC arrow `\e\e[C`).
            // `\e\e[C` is intentionally excluded from `vi_insertion_keymap` so pressing ESC
            // in Vi insert mode never stalls on an ESC-ESC sub-keymap.
            bind_and_record(&mut st, emacs_map, b"\x06\0", accept_or_ctrl_f);
            bind_and_record(&mut st, emacs_map, b"\x1bf\0", accept_word_or_alt_f);
            bind_and_record(
                &mut st,
                emacs_map,
                b"\x1b\x1b[C\0",
                accept_word_or_alt_right,
            );
        }

        st.enabled = true;
    });
}

/// Disable autosuggest hooks and restore all Readline callbacks and keybindings.
pub fn disable() {
    // SAFETY: Called from Bash's builtin dispatcher or unload hook on Bash's main thread.
    unsafe { clear_ghost_if_drawn() };

    STATE.with(|cell| {
        let Ok(mut st) = cell.try_borrow_mut() else {
            return;
        };
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
        cell.try_borrow()
            .map(|st| (st.enabled, st.style_str().to_owned()))
            .unwrap_or_else(|_| (false, DEFAULT_SGR_STYLE.to_owned()))
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
