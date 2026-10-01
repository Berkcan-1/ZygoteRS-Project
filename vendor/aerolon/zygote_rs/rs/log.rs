//! Minimal logcat bridge. Tag is "Zygote" so output stays next to the C++ ALOGx lines.
//! NOT async-signal-safe: never call from the SIGCHLD handler (that stays in C++).

pub const DEBUG: i32 = 3;
pub const WARN: i32 = 5;
pub const ERROR: i32 = 6;

#[cfg(target_os = "android")]
pub fn write(prio: i32, msg: &str) {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};
    extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }
    if let Ok(text) = CString::new(msg) {
        unsafe {
            __android_log_write(prio, b"Zygote\0".as_ptr() as *const c_char, text.as_ptr());
        }
    }
}

#[cfg(not(target_os = "android"))]
pub fn write(prio: i32, msg: &str) {
    eprintln!("[zygote_rs:{}] {}", prio, msg);
}

pub fn d(msg: &str) {
    write(DEBUG, msg)
}
pub fn w(msg: &str) {
    write(WARN, msg)
}
pub fn e(msg: &str) {
    write(ERROR, msg)
}
