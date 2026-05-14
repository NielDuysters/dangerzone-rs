//! Raw Tesseract C API calls that are not currently surfaced by
//! `kreuzberg-tesseract`'s safe Rust API.

use std::os::raw::{c_char, c_int, c_void};

unsafe extern "C-unwind" {
    pub(super) fn TessDeleteText(text: *mut c_char);
    pub(super) fn TessPageIteratorBegin(handle: *mut c_void);
    pub(super) fn TessPageIteratorIsAtBeginningOf(handle: *mut c_void, level: c_int) -> c_int;
    pub(super) fn TessPageIteratorBoundingBox(
        handle: *mut c_void,
        level: c_int,
        left: *mut c_int,
        top: *mut c_int,
        right: *mut c_int,
        bottom: *mut c_int,
    ) -> c_int;
    pub(super) fn TessResultIteratorGetUTF8Text(handle: *mut c_void, level: c_int) -> *mut c_char;
    pub(super) fn TessResultIteratorNext(handle: *mut c_void, level: c_int) -> c_int;
}
