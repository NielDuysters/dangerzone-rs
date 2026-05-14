//! Raw Tesseract C API calls that are not currently surfaced by
//! `kreuzberg-tesseract`'s safe Rust API.

use std::os::raw::{c_char, c_int, c_void};

unsafe extern "C-unwind" {
    /// Free strings returned by Tesseract, such as `TessResultIteratorGetUTF8Text`.
    pub(super) fn TessDeleteText(text: *mut c_char);

    /// Move the page iterator back to the first recognized element.
    pub(super) fn TessPageIteratorBegin(handle: *mut c_void);

    /// Return non-zero when the iterator is at the first element of `level`.
    ///
    /// We use this to increment block and line ids while walking words.
    pub(super) fn TessPageIteratorIsAtBeginningOf(handle: *mut c_void, level: c_int) -> c_int;

    /// Write the bounding box for the current iterator element at `level`.
    ///
    /// Coordinates are returned as left, top, right, bottom in image pixels.
    pub(super) fn TessPageIteratorBoundingBox(
        handle: *mut c_void,
        level: c_int,
        left: *mut c_int,
        top: *mut c_int,
        right: *mut c_int,
        bottom: *mut c_int,
    ) -> c_int;

    /// Return recognized UTF-8 text for the current iterator element at `level`.
    ///
    /// The returned pointer must be released with `TessDeleteText`.
    pub(super) fn TessResultIteratorGetUTF8Text(handle: *mut c_void, level: c_int) -> *mut c_char;

    /// Advance the result iterator to the next element at `level`.
    ///
    /// Returns zero when there is no next element.
    pub(super) fn TessResultIteratorNext(handle: *mut c_void, level: c_int) -> c_int;
}
