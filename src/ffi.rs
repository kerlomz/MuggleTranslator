use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::pipeline::{PipelineConfig, TranslatorPipeline};
use crate::progress::ConsoleProgress;

static LAST_ERROR: Lazy<Mutex<Option<CString>>> = Lazy::new(|| Mutex::new(None));

fn set_last_error(msg: &str) {
    let c = CString::new(msg).unwrap_or_else(|_| CString::new("error").expect("cstr"));
    let mut guard = LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(c);
}

fn take_cstr(ptr: *const c_char, name: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("{name} is null"));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|s| s.to_string())
        .map_err(|_| format!("{name} is not valid UTF-8"))
}

/// Translate a DOCX file using `muggle-translator.toml` and built-in pipeline.
///
/// Returns 0 on success; non-zero on failure (see `mt_last_error_utf8()`).
#[no_mangle]
pub extern "C" fn mt_translate_docx(
    config_path: *const c_char,
    input_docx: *const c_char,
    output_docx: *const c_char,
) -> i32 {
    let cfg = match take_cstr(config_path, "config_path") {
        Ok(v) => v,
        Err(e) => {
            set_last_error(&e);
            return 2;
        }
    };
    let input = match take_cstr(input_docx, "input_docx") {
        Ok(v) => v,
        Err(e) => {
            set_last_error(&e);
            return 3;
        }
    };
    let output = match take_cstr(output_docx, "output_docx") {
        Ok(v) => v,
        Err(e) => {
            set_last_error(&e);
            return 4;
        }
    };

    let input = PathBuf::from(input);
    let output = PathBuf::from(output);
    let cfg_path = PathBuf::from(cfg);

    let progress = ConsoleProgress::new(false);
    let cfg = match PipelineConfig::from_paths_and_args(
        &input,
        &output,
        Some(cfg_path),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(&format!("{err:#}"));
            return 10;
        }
    };

    let mut pipeline = TranslatorPipeline::new(cfg, progress);
    match pipeline.translate_docx(&input, &output) {
        Ok(()) => 0,
        Err(err) => {
            set_last_error(&format!("{err:#}"));
            11
        }
    }
}

/// Returns the last error message as a UTF-8 C string pointer (or null if none).
/// The pointer is valid until the next `mt_translate_docx` call.
#[no_mangle]
pub extern "C" fn mt_last_error_utf8() -> *const c_char {
    let guard = LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}
