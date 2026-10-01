//! Fixed-size error buffer shared with the C++ glue (`ZrsError` in zygote_rs.h).
//!
//! Every fallible FFI function reports failure as `false` plus a message in this
//! struct; the C++ side then forwards the message to `ZygoteFailure()`, exactly
//! like the old `fail_fn(CREATE_ERROR(...))` calls did.

use std::ffi::CStr;
use std::fmt::{self, Write};

pub const ZRS_ERR_MSG_LEN: usize = 256;

#[repr(C)]
pub struct ZrsError {
    pub errnum: i32,
    pub msg: [u8; ZRS_ERR_MSG_LEN],
}

struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.pos;
        let n = s.len().min(room);
        self.buf[self.pos..self.pos + n].copy_from_slice(&s.as_bytes()[..n]);
        self.pos += n;
        Ok(()) // silently truncate; a cut-off message is better than none
    }
}

impl ZrsError {
    pub fn set(&mut self, errnum: i32, args: fmt::Arguments<'_>) {
        self.errnum = errnum;
        let pos = {
            let mut w = Cursor { buf: &mut self.msg[..ZRS_ERR_MSG_LEN - 1], pos: 0 };
            let _ = w.write_fmt(args);
            w.pos
        };
        self.msg[pos] = 0;
    }
}

/// Current thread's errno.
pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Thread-safe strerror (XSI `strerror_r`), matching what `strerror()` printed in the C++ code.
pub fn strerror(errnum: i32) -> String {
    let mut buf = [0 as libc::c_char; 128];
    let rc = unsafe { libc::strerror_r(errnum, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return format!("errno {}", errnum);
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_nul_terminated_and_truncated() {
        let mut e = ZrsError { errnum: 0, msg: [0xAA; ZRS_ERR_MSG_LEN] };
        e.set(5, format_args!("hello {}", 42));
        assert_eq!(&e.msg[..9], b"hello 42\0");
        assert_eq!(e.errnum, 5);

        let long = "x".repeat(1000);
        e.set(0, format_args!("{}", long));
        assert_eq!(e.msg[ZRS_ERR_MSG_LEN - 1], 0);
        assert_eq!(e.msg[ZRS_ERR_MSG_LEN - 2], b'x');
    }

    #[test]
    fn strerror_is_not_empty() {
        assert!(!strerror(libc::EPERM).is_empty());
    }
}
