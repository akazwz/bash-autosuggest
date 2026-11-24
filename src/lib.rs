//! `bash-autosuggest`: Native GNU Readline history autosuggestions for Bash.
//!
//! Load inside an interactive Bash session via:
//! ```bash
//! enable -f ~/.local/lib/bash_autosuggest.so bash_autosuggest
//! bash_autosuggest
//! ```

pub mod ffi;
pub mod hook;
#[forbid(unsafe_code)]
pub mod suggest;

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_int};
use std::ptr;

use crate::ffi::{BUILTIN_ENABLED, BashBuiltin, EXECUTION_FAILURE, EXECUTION_SUCCESS, WordList};

const BUILTIN_NAME: &CStr = c"bash_autosuggest";
const SHORT_DOC: &CStr = c"bash_autosuggest [enable|disable|status]";
const DOC_LINE_1: &CStr =
    c"Native GNU Readline history autosuggestions for Bash (ghost text + Right Arrow accept).";
const DOC_LINE_2: &CStr =
    c"Subcommands: enable (default), disable, status. Configure color with BASH_AUTOSUGGEST_STYLE.";

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

unsafe fn first_arg(list: *mut WordList) -> Option<&'static str> {
    // SAFETY: Caller guarantees `list` is null or a valid Bash `WordList` node whose strings
    // live for the duration of the builtin call.
    unsafe {
        if list.is_null() {
            return None;
        }
        let word_desc = (*list).word;
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

/// Main builtin entrypoint: `bash_autosuggest [enable|disable|status]`.
///
/// # Safety
/// Must only be invoked by Bash's builtin dispatcher with either a null pointer
/// or a valid linked list of `WordList` nodes allocated by Bash.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bash_autosuggest_builtin(list: *mut WordList) -> c_int {
    // SAFETY: `list` validity is upheld by the caller (Bash builtin dispatcher).
    let subcmd = unsafe { first_arg(list) }.unwrap_or("enable");
    match subcmd {
        "enable" | "on" => {
            hook::enable();
            EXECUTION_SUCCESS
        }
        "disable" | "off" => {
            hook::disable();
            EXECUTION_SUCCESS
        }
        "status" => {
            println!("{}", hook::status());
            EXECUTION_SUCCESS
        }
        other => {
            eprintln!(
                "bash_autosuggest: unknown subcommand '{other}' (expected enable|disable|status)"
            );
            EXECUTION_FAILURE
        }
    }
}
