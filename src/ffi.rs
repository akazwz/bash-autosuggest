//! Low-level FFI declarations for Bash 5.2 loadable builtins, GNU Readline 8.2,
//! and minimal C stdio helpers.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_ulong, c_void};

/// Bash `BUILTIN_ENABLED` flag from `builtins.h`.
pub const BUILTIN_ENABLED: c_int = 0x01;

/// Bash builtin return codes.
pub const EXECUTION_SUCCESS: c_int = 0;
pub const EXECUTION_FAILURE: c_int = 1;

/// Readline state bitflags from GNU Readline `readline.h`.
pub const RL_STATE_ISEARCH: c_ulong = 0x0000_0080;
pub const RL_STATE_NSEARCH: c_ulong = 0x0000_0100;
pub const RL_STATE_SEARCH: c_ulong = 0x0000_0200;
pub const RL_STATE_NUMERICARG: c_ulong = 0x0000_0400;
pub const RL_STATE_MACROINPUT: c_ulong = 0x0000_0800;
pub const RL_STATE_OVERWRITE: c_ulong = 0x0000_2000;
pub const RL_STATE_COMPLETING: c_ulong = 0x0000_4000;
pub const RL_STATE_SIGHANDLER: c_ulong = 0x0000_8000;
pub const RL_STATE_INPUTPENDING: c_ulong = 0x0002_0000;
pub const RL_STATE_VIMOTION: c_ulong = 0x0010_0000;
pub const RL_STATE_CHARSEARCH: c_ulong = 0x0080_0000;
pub const RL_STATE_DONE: c_ulong = 0x0200_0000;

/// Combined mask of Readline states in which ghost text must not be rendered.
pub const RL_STATE_SUPPRESS_MASK: c_ulong = RL_STATE_ISEARCH
    | RL_STATE_NSEARCH
    | RL_STATE_SEARCH
    | RL_STATE_NUMERICARG
    | RL_STATE_MACROINPUT
    | RL_STATE_OVERWRITE
    | RL_STATE_COMPLETING
    | RL_STATE_SIGHANDLER
    | RL_STATE_INPUTPENDING
    | RL_STATE_VIMOTION
    | RL_STATE_CHARSEARCH
    | RL_STATE_DONE;

/// Readline keymap entry types from `keymaps.h`.
pub const ISFUNC: c_int = 0;
pub const ISKMAP: c_int = 1;
pub const ISMACR: c_int = 2;

/// Opaque Readline keymap pointer (`Keymap`).
pub type Keymap = *mut c_void;

/// Readline command callback (`rl_command_func_t`).
pub type RlCommandFunc = unsafe extern "C" fn(c_int, c_int) -> c_int;

/// Readline void callback (`rl_voidfunc_t`, used by `rl_redisplay_function`).
pub type RlVoidFunc = unsafe extern "C" fn();

/// Readline hook callback (`rl_hook_func_t`, used by `rl_pre_input_hook`).
pub type RlHookFunc = unsafe extern "C" fn() -> c_int;

/// Bash `WORD_DESC` from `command.h`.
#[repr(C)]
pub struct WordDesc {
    pub word: *mut c_char,
    pub flags: c_int,
}

/// Bash `WORD_LIST` from `command.h`.
#[repr(C)]
pub struct WordList {
    pub next: *mut WordList,
    pub word: *mut WordDesc,
}

/// Bash builtin entry function (`sh_builtin_func_t`).
pub type ShBuiltinFunc = unsafe extern "C" fn(*mut WordList) -> c_int;

/// Bash `struct builtin` from `builtins.h` (ABI-stable across Bash 5.x on x86_64).
#[repr(C)]
pub struct BashBuiltin {
    pub name: *const c_char,
    pub function: ShBuiltinFunc,
    pub flags: c_int,
    pub long_doc: *const *const c_char,
    pub short_doc: *const c_char,
    pub handle: *mut c_char,
}

// SAFETY: `BashBuiltin` holds static C string pointers and function pointers initialized
// at compile time and only read/mutated by Bash's single-threaded builtin loader.
unsafe impl Sync for BashBuiltin {}

/// GNU Readline `HIST_ENTRY` from `history.h`.
#[repr(C)]
pub struct HistEntry {
    pub line: *mut c_char,
    pub timestamp: *mut c_char,
    pub data: *mut c_void,
}

/// Opaque C `FILE` handle.
#[repr(C)]
pub struct File {
    _private: [u8; 0],
}

unsafe extern "C" {
    // --- GNU Readline state & display ---
    pub static mut rl_line_buffer: *mut c_char;
    pub static mut rl_point: c_int;
    pub static mut rl_end: c_int;
    pub static mut rl_done: c_int;
    pub static mut rl_readline_state: c_ulong;
    pub static mut rl_redisplay_function: Option<RlVoidFunc>;
    pub static mut rl_pre_input_hook: Option<RlHookFunc>;
    pub static mut rl_outstream: *mut File;
    pub static mut rl_display_prompt: *const c_char;
    pub static mut rl_prompt: *const c_char;

    // --- GNU Readline keymaps ---
    pub static mut emacs_standard_keymap: c_void;
    pub static mut vi_insertion_keymap: c_void;
    pub static mut vi_movement_keymap: c_void;

    // --- GNU Readline functions ---
    pub fn rl_get_keymap() -> Keymap;
    pub fn rl_redisplay();
    pub fn rl_insert_text(text: *const c_char) -> c_int;
    pub fn rl_forward_char(count: c_int, key: c_int) -> c_int;
    pub fn rl_end_of_line(count: c_int, key: c_int) -> c_int;
    pub fn rl_forward_word(count: c_int, key: c_int) -> c_int;
    pub fn rl_newline(count: c_int, key: c_int) -> c_int;
    pub fn rl_get_screen_size(rows: *mut c_int, cols: *mut c_int);
    pub fn rl_bind_keyseq_in_map(
        keyseq: *const c_char,
        function: Option<RlCommandFunc>,
        map: Keymap,
    ) -> c_int;
    pub fn rl_function_of_keyseq_len(
        keyseq: *const c_char,
        len: usize,
        map: Keymap,
        type_out: *mut c_int,
    ) -> Option<RlCommandFunc>;

    // --- GNU Readline history ---
    pub static mut history_length: c_int;
    pub fn history_list() -> *mut *mut HistEntry;

    // --- C stdio ---
    pub fn fwrite(ptr: *const c_void, size: usize, nmemb: usize, stream: *mut File) -> usize;
    pub fn fflush(stream: *mut File) -> c_int;
}
