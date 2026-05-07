//! Raw Tesseract C API calls not exposed by `kreuzberg-tesseract`.

use std::os::raw::{c_char, c_int, c_void};

unsafe extern "C-unwind" {
    pub(super) fn TessDeleteText(text: *mut c_char);
    pub(super) fn TessPageIteratorBegin(handle: *mut c_void);
    pub(super) fn TessPageIteratorIsAtBeginningOf(handle: *mut c_void, level: c_int) -> c_int;
    pub(super) fn TessPageIteratorIsAtFinalElement(
        handle: *mut c_void,
        level: c_int,
        element: c_int,
    ) -> c_int;
    pub(super) fn TessPageIteratorBoundingBox(
        handle: *mut c_void,
        level: c_int,
        left: *mut c_int,
        top: *mut c_int,
        right: *mut c_int,
        bottom: *mut c_int,
    ) -> c_int;
    pub(super) fn TessPageIteratorBaseline(
        handle: *mut c_void,
        level: c_int,
        x1: *mut c_int,
        y1: *mut c_int,
        x2: *mut c_int,
        y2: *mut c_int,
    ) -> c_int;
    pub(super) fn TessPageIteratorOrientation(
        handle: *mut c_void,
        orientation: *mut c_int,
        writing_direction: *mut c_int,
        textline_order: *mut c_int,
        deskew_angle: *mut f32,
    );
    pub(super) fn TessResultIteratorGetUTF8Text(handle: *mut c_void, level: c_int) -> *mut c_char;
    pub(super) fn TessResultIteratorNext(handle: *mut c_void, level: c_int) -> c_int;
    pub(super) fn TessResultIteratorWordFontAttributes(
        handle: *mut c_void,
        is_bold: *mut c_int,
        is_italic: *mut c_int,
        is_underlined: *mut c_int,
        is_monospace: *mut c_int,
        is_serif: *mut c_int,
        is_smallcaps: *mut c_int,
        pointsize: *mut c_int,
        font_id: *mut c_int,
    ) -> c_int;
}
