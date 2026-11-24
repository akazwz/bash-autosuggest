//! `bash-autosuggest`: Native GNU Readline history autosuggestions & real-time syntax highlighting for Bash.
//!
//! Load inside an interactive Bash session via:
//! ```bash
//! enable -f ~/.local/lib/bash_autosuggest.so bash_autosuggest
//! bash_autosuggest
//! ```

pub mod ffi;
#[forbid(unsafe_code)]
pub mod highlight;
pub mod hook;
#[forbid(unsafe_code)]
pub mod suggest;

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_int};
use std::ptr;

use crate::ffi::{BUILTIN_ENABLED, BashBuiltin, EXECUTION_FAILURE, EXECUTION_SUCCESS, WordList};

const BUILTIN_NAME: &CStr = c"bash_autosuggest";
const SHORT_DOC: &CStr = c"bash_autosuggest [enable|disable|status|highlight [on|off]]";
const DOC_LINE_1: &CStr =
    c"Native GNU Readline history autosuggestions & syntax highlighting for Bash.";
const DOC_LINE_2: &CStr = c"Subcommands: enable (default), disable, status, highlight [on|off]. Env: BASH_AUTOSUGGEST_STYLE, BASH_AUTOSUGGEST_HIGHLIGHT.";

struct SyncDocArray([*const c_char; 3]);

// SAFETY: Static C string pointers only.
unsafe impl Sync for SyncDocArray {}

static LONG_DOC: SyncDocArray =
    SyncDocArray([DOC_LINE_1.as_ptr(), DOC_LINE_2.as_ptr(), ptr::null()]);

/// Wrapper that places `BashBuiltin` in writable `.data` rather than `.rodata`,
/// because Bash's `enable -f` mutates `struct builtin::handle` and `flags` in place.
#[repr(transparent)]
pub struct WritableBuiltin(UnsafeCell<BashBuiltin>);

// SAFETY: Mutated solely by Bash's single-threaded builtin loader/unloader.
unsafe impl Sync for WritableBuiltin {}

/// Exported Bash loadable builtin descriptor (`bash_autosuggest_struct`).
#[unsafe(no_mangle)]
pub static bash_autosuggest_struct: WritableBuiltin =
    WritableBuiltin(UnsafeCell::new(BashBuiltin {
        name: BUILTIN_NAME.as_ptr(),
        function: bash_autosuggest_builtin,
        flags: BUILTIN_ENABLED,
        long_doc: LONG_DOC.0.as_ptr(),
        short_doc: SHORT_DOC.as_ptr(),
        handle: ptr::null_mut(),
    }));

/// Called automatically by Bash when `enable -f ... bash_autosuggest` loads the `.so`.
#[unsafe(no_mangle)]
pub extern "C" fn bash_autosuggest_builtin_load(_name: *mut c_char) -> c_int {
    1
}

/// Called automatically by Bash when `enable -d bash_autosuggest` unloads the builtin.
#[unsafe(no_mangle)]
pub extern "C" fn bash_autosuggest_builtin_unload(_name: *mut c_char) {
    hook::disable();
}

/// Read the word string from a non-null Bash `WordList` node.
///
/// # Safety
/// `list` must reference a valid Bash `WordList` whose `word` string remains valid
/// for the lifetime of the borrow.
unsafe fn node_word(list: &WordList) -> Option<&str> {
    // SAFETY: Caller guarantees `list` and its nested pointers are valid for the borrow.
    unsafe {
        let word_desc = list.word;
        if word_desc.is_null() {
            return None;
        }
        let word_ptr = (*word_desc).word;
        if word_ptr.is_null() {
            return None;
        }
        CStr::from_ptr(word_ptr).to_str().ok()
    }
}

/// Main builtin entrypoint: `bash_autosuggest [enable|disable|status|highlight [on|off]]`.
///
/// # Safety
/// Must only be invoked by Bash's builtin dispatcher with either a null pointer
/// or a valid linked list of `WordList` nodes allocated by Bash.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bash_autosuggest_builtin(list: *mut WordList) -> c_int {
    // SAFETY: `list` validity is upheld by the caller (Bash builtin dispatcher).
    let (subcmd, arg2) = unsafe {
        match list.as_ref() {
            Some(node) => {
                let first = node_word(node);
                let second = node.next.as_ref().and_then(|next| node_word(next));
                (first, second)
            }
            None => (None, None),
        }
    };
    match subcmd.unwrap_or("enable") {
        "enable" | "on" => {
            hook::enable();
            EXECUTION_SUCCESS
        }
        "disable" | "off" => {
            hook::disable();
            EXECUTION_SUCCESS
        }
        "highlight" => match arg2.unwrap_or("on") {
            "on" | "enable" | "1" | "true" => {
                hook::set_highlight(true);
                EXECUTION_SUCCESS
            }
            "off" | "disable" | "0" | "false" => {
                hook::set_highlight(false);
                EXECUTION_SUCCESS
            }
            other => {
                eprintln!(
                    "bash_autosuggest: unknown highlight setting '{other}' (expected on|off)"
                );
                EXECUTION_FAILURE
            }
        },
        "status" => {
            println!("{}", hook::status());
            EXECUTION_SUCCESS
        }
        other => {
            eprintln!(
                "bash_autosuggest: unknown subcommand '{other}' (expected enable|disable|status|highlight)"
            );
            EXECUTION_FAILURE
        }
    }
}
