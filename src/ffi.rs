//! Bindings to libav and the kernel's CEC interface, generated from their
//! own headers.
//!
//! Nothing here is transcribed by hand. The structs libav exposes are fully
//! declared in its headers, and every field offset in them differs between a
//! 64-bit and a 32-bit Pi, so generating them is what makes the same source
//! correct on both boards. See build.rs.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
// The bindings are generated: nothing here is written by hand, so there is
// nothing for a lint to tell anyone.
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use std::os::raw::c_int;

/// Turn a libav error code into something readable.
pub fn av_err(code: c_int) -> String {
    let mut buf = [0 as std::os::raw::c_char; 256];
    unsafe {
        av_strerror(code, buf.as_mut_ptr(), buf.len());
        std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

// AVERROR() is a macro, so bindgen emits neither of these. They are the two
// non-errors a decode loop sees constantly: more input needed, and end of
// stream.
pub const AVERROR_EAGAIN: c_int = -11;
pub const AVERROR_EOF: c_int = -541478725; // FFERRTAG('E','O','F',' ')
