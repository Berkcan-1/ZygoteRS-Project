//! System property lookup used by the command-buffer parser (`wrap.<nice-name>` check).

use std::ffi::CStr;

#[cfg(target_os = "android")]
pub fn exists(name: &CStr) -> bool {
    use std::os::raw::{c_char, c_void};
    extern "C" {
        // bionic libc: <sys/system_properties.h>
        fn __system_property_find(name: *const c_char) -> *const c_void;
    }
    unsafe { !__system_property_find(name.as_ptr()).is_null() }
}

#[cfg(not(target_os = "android"))]
pub fn exists(_name: &CStr) -> bool {
    false
}
