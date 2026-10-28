//! C ABI surface — everything declared in include/fileopskit.h.
//! Conventions (house standard):
//! - `*mut c_char` JSON results are caller-owned; free with
//!   `fo_free_string`.
//! - Hard failures (bad arguments, unreadable spec/manifest)
//!   return null and set the thread-local last error.
//! - Per-item job failures are NOT hard failures: `fo_run`
//!   returns a result JSON whose "state" field reports them.
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::catch_unwind;

use crate::{ERR_INTERNAL, ERR_INVALID_ARG, FO_OK};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(FO_OK, "");
}

fn out_json(result: crate::FoResult<String>) -> *mut c_char {
    match result {
        Ok(text) => match CString::new(text) {
            Ok(s) => {
                clear_error();
                s.into_raw()
            }
            Err(_) => {
                set_error(ERR_INTERNAL, "result string contained a NUL byte");
                std::ptr::null_mut()
            }
        },
        Err((code, msg)) => {
            set_error(code, &msg);
            std::ptr::null_mut()
        }
    }
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> crate::FoResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

fn guard_json<F: FnOnce() -> *mut c_char>(f: F) -> *mut c_char {
    match catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(p) => p,
        Err(_) => {
            set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn fo_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn fo_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn fo_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this library
/// returned and has not been freed yet. Documented in the header.
#[no_mangle]
pub extern "C" fn fo_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

#[no_mangle]
pub extern "C" fn fo_plan(spec_json: *const c_char) -> *mut c_char {
    guard_json(|| {
        let inner = cstr(spec_json, "spec_json").and_then(crate::plan);
        out_json(inner)
    })
}

/// Progress callback type matching the header's `FoProgressFn`.
pub type FoProgressFn = extern "C" fn(*mut c_void, *const c_char) -> c_int;

#[no_mangle]
pub extern "C" fn fo_run(
    spec_json: *const c_char,
    progress_cb: Option<FoProgressFn>,
    user_data: *mut c_void,
) -> *mut c_char {
    guard_json(|| {
        let inner = (|| -> crate::FoResult<String> {
            let text = cstr(spec_json, "spec_json")?;
            match progress_cb {
                None => crate::run(text, None),
                Some(cb) => {
                    let mut wrapper = |ev: &crate::ProgressEvent| -> bool {
                        let json = ev.to_json();
                        match CString::new(json) {
                            Ok(c) => {
                                // The callback is host code; if it
                                // misbehaves the engine cannot see
                                // it — the contract (header) is
                                // that it returns promptly.
                                cb(user_data, c.as_ptr()) == 0
                            }
                            Err(_) => true,
                        }
                    };
                    crate::run(text, Some(&mut wrapper))
                }
            }
        })();
        out_json(inner)
    })
}

#[no_mangle]
pub extern "C" fn fo_undo(manifest_path: *const c_char) -> *mut c_char {
    guard_json(|| {
        let inner = cstr(manifest_path, "manifest_path").and_then(crate::undo);
        out_json(inner)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn ffi_plan_and_free() {
        let dir = std::env::temp_dir().join(format!("fileopskit-ffi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), b"hi").unwrap();
        let spec = format!(
            "{{\"op\":\"copy\",\"sources\":[\"{}\"],\"dest_dir\":\"{}\"}}",
            dir.join("src").to_string_lossy().replace('\\', "\\\\"),
            dir.join("out").to_string_lossy().replace('\\', "\\\\")
        );
        let p = fo_plan(cs(&spec).as_ptr());
        assert!(!p.is_null());
        let text = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        assert!(text.contains("\"item_count\":1"), "{text}");
        fo_free_string(p);
        assert_eq!(fo_last_error_code(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffi_null_and_bad_spec() {
        assert!(fo_plan(std::ptr::null()).is_null());
        assert_eq!(fo_last_error_code(), ERR_INVALID_ARG);
        assert!(fo_undo(cs("/no/such/manifest.json").as_ptr()).is_null());
        assert_eq!(fo_last_error_code(), crate::ERR_NOT_FOUND);
        fo_free_string(std::ptr::null_mut()); // must be a no-op
    }

    extern "C" fn cancel_cb(_ud: *mut c_void, _ev: *const c_char) -> c_int {
        1 // cancel at the very first event
    }

    #[test]
    fn ffi_run_with_cancelling_callback() {
        let dir =
            std::env::temp_dir().join(format!("fileopskit-ffi-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"data").unwrap();
        let spec = format!(
            "{{\"op\":\"copy\",\"sources\":[\"{}\"],\"dest_dir\":\"{}\",\"undo_dir\":\"{}\"}}",
            dir.join("a.txt").to_string_lossy().replace('\\', "\\\\"),
            dir.join("out").to_string_lossy().replace('\\', "\\\\"),
            dir.join("undo").to_string_lossy().replace('\\', "\\\\")
        );
        let p = fo_run(cs(&spec).as_ptr(), Some(cancel_cb), std::ptr::null_mut());
        assert!(!p.is_null());
        let text = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        assert!(text.contains("\"state\":\"cancelled\""), "{text}");
        fo_free_string(p);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
