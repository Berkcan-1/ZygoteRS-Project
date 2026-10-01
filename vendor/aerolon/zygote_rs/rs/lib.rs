//! zygote_rs — Rust replacements for the leaf logic of Zygote's JNI layer (AOSP 13).
//!
//! Design rules (see README.md):
//!  * NO JNI in Rust. The C++ glue keeps every `JNIEnv*` interaction and hands us plain
//!    pointers/lengths, so there is no `jni` crate dependency and no JNI lifetime subtleties.
//!  * fork(), the SIGCHLD handler, SELinux (`selinux_android_*`, `setfilecon`), seccomp,
//!    mallopt/GWP-ASan and the mount-namespace code stay in C++ for this phase, and the
//!    call ORDER inside SpecializeCommon() is unchanged.
//!  * Fallible calls return `bool` and fill a `ZrsError`; C++ forwards the message to
//!    `ZygoteFailure()` (=> `JNIEnv::FatalError`), so failure behaviour is unchanged.
//!  * Everything here must be built with panic=abort (Soong default on device): unwinding
//!    across `extern "C"` would be UB.

#![allow(clippy::missing_safety_doc)]

mod caps;
mod cmdbuf;
mod errbuf;
mod log;
mod props;
mod sigchld;
mod usap;

use std::os::raw::c_char;

use cmdbuf::CommandBuffer;
use errbuf::ZrsError;

unsafe fn slice_or_empty<'a, T>(p: *const T, len: usize) -> &'a [T] {
    if p.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(p, len)
    }
}

/// Stores `msg` into `err` (if given) and returns false, so callers can `return fail(..)`.
unsafe fn fail(err: *mut ZrsError, msg: &str) -> bool {
    if !err.is_null() {
        (*err).set(0, format_args!("{}", msg));
    }
    false
}

unsafe fn done(err: *mut ZrsError, r: Result<(), String>) -> bool {
    match r {
        Ok(()) => true,
        Err(m) => fail(err, &m),
    }
}

// ───────────────────────────── USAP table ─────────────────────────────

#[no_mangle]
pub extern "C" fn zrs_usap_add(pid: i32, read_pipe_fd: i32) -> bool {
    usap::add(pid, read_pipe_fd)
}

/// ASYNC-SIGNAL-SAFE (called from the SIGCHLD handler).
#[no_mangle]
pub extern "C" fn zrs_usap_remove(pid: i32) -> bool {
    usap::remove(pid)
}

#[no_mangle]
pub extern "C" fn zrs_usap_count() -> u32 {
    usap::count()
}

#[no_mangle]
pub extern "C" fn zrs_usap_clear_all() {
    usap::clear_all()
}

#[no_mangle]
pub extern "C" fn zrs_usap_empty_pool() {
    usap::empty_pool()
}

#[no_mangle]
pub unsafe extern "C" fn zrs_usap_read_fds(out: *mut i32, cap: usize) -> usize {
    if out.is_null() || cap == 0 {
        return 0;
    }
    usap::read_fds(std::slice::from_raw_parts_mut(out, cap))
}

// ───────────────────────────── SIGCHLD message ─────────────────────────────

/// Returns 3 and fills `out[0..3] = {pid, uid, status}` for a valid message, -1 otherwise.
#[no_mangle]
pub unsafe extern "C" fn zrs_parse_sigchld(data: *const u8, len: usize, out: *mut i32) -> i32 {
    if data.is_null() || out.is_null() {
        return -1;
    }
    match sigchld::parse(slice_or_empty(data, len)) {
        Some(v) => {
            for (i, x) in v.iter().enumerate() {
                *out.add(i) = *x;
            }
            3
        }
        None => -1,
    }
}

// ───────────────────────────── capabilities / creds ─────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn zrs_calculate_capabilities(
    uid: i32,
    gid: i32,
    gids: *const i32,
    gids_len: usize,
    has_gids: bool,
    is_child_zygote: bool,
    out: *mut u64,
    err: *mut ZrsError,
) -> bool {
    if out.is_null() {
        return fail(err, "zrs_calculate_capabilities: null out");
    }
    let mask = match caps::effective_mask() {
        Ok(m) => m,
        Err(e) => return fail(err, &e),
    };
    let g = if has_gids { Some(slice_or_empty(gids, gids_len)) } else { None };
    *out = caps::calculate(uid, gid, g, is_child_zygote, mask);
    true
}

#[no_mangle]
pub unsafe extern "C" fn zrs_keep_capabilities(err: *mut ZrsError) -> bool {
    done(err, caps::enable_keep_capabilities())
}

#[no_mangle]
pub unsafe extern "C" fn zrs_drop_bounding_set(err: *mut ZrsError) -> bool {
    done(err, caps::drop_bounding_set())
}

#[no_mangle]
pub unsafe extern "C" fn zrs_set_inheritable(inheritable: u64, err: *mut ZrsError) -> bool {
    done(err, caps::set_inheritable(inheritable))
}

#[no_mangle]
pub unsafe extern "C" fn zrs_set_capabilities(
    permitted: u64,
    effective: u64,
    inheritable: u64,
    err: *mut ZrsError,
) -> bool {
    done(err, caps::set_capabilities(permitted, effective, inheritable))
}

#[no_mangle]
pub unsafe extern "C" fn zrs_set_gids(
    gids: *const i32,
    gids_len: usize,
    has_gids: bool,
    is_child_zygote: bool,
    err: *mut ZrsError,
) -> bool {
    let g = if has_gids { Some(slice_or_empty(gids, gids_len)) } else { None };
    done(err, caps::set_gids(g, is_child_zygote))
}

#[no_mangle]
pub unsafe extern "C" fn zrs_set_rlimits(triples: *const i32, len: usize, err: *mut ZrsError) -> bool {
    done(err, caps::set_rlimits(slice_or_empty(triples, len)))
}

// ───────────────────────────── command buffer ─────────────────────────────
// `ZrsCmdBuf*` is an opaque pointer to a `CommandBuffer` living in its own mmap.

type ZrsCmdBuf = CommandBuffer;

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_new(fd: i32) -> *mut ZrsCmdBuf {
    CommandBuffer::alloc(fd)
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_free(b: *mut ZrsCmdBuf) -> bool {
    !b.is_null() && CommandBuffer::free(b)
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_get_count(b: *mut ZrsCmdBuf, out: *mut i32, err: *mut ZrsError) -> bool {
    match (*b).get_count() {
        Ok(n) => {
            *out = n;
            true
        }
        Err(m) => fail(err, &m),
    }
}

/// On success `*out_ptr`/`*out_len` describe the next argument. The bytes are NOT
/// NUL-terminated and stay valid only until the next call on this buffer.
#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_next_arg(
    b: *mut ZrsCmdBuf,
    out_ptr: *mut *const c_char,
    out_len: *mut usize,
    err: *mut ZrsError,
) -> bool {
    match (*b).read_line() {
        Ok(Some((s, e))) => {
            *out_ptr = (*b).line_ptr(s);
            *out_len = e - s;
            true
        }
        Ok(None) => fail(err, "Incomplete zygote command"),
        Err(m) => fail(err, &m),
    }
}

/// Read the rest of the current command into the buffer, then rewind to its start.
#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_read_fully_and_reset(b: *mut ZrsCmdBuf, err: *mut ZrsError) -> bool {
    match (*b).read_all_lines() {
        Ok(()) => {
            (*b).reset();
            true
        }
        Err(m) => fail(err, &m),
    }
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_insert(
    b: *mut ZrsCmdBuf,
    line: *const u8,
    len: usize,
    err: *mut ZrsError,
) -> bool {
    done(err, (*b).insert(slice_or_empty(line, len)))
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_is_simple_fork(
    b: *mut ZrsCmdBuf,
    min_uid: i32,
    out: *mut bool,
    err: *mut ZrsError,
) -> bool {
    match (*b).is_simple_fork_command(min_uid) {
        Ok(v) => {
            *out = v;
            true
        }
        Err(m) => fail(err, &m),
    }
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_reset(b: *mut ZrsCmdBuf) {
    (*b).reset()
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_clear(b: *mut ZrsCmdBuf) {
    (*b).clear()
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_fd(b: *const ZrsCmdBuf) -> i32 {
    (*b).fd()
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_set_fd(b: *mut ZrsCmdBuf, fd: i32) {
    (*b).set_fd(fd)
}

/// NUL-terminated nice name (possibly empty). Pointer is stable for the buffer's lifetime.
#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_nice_name(b: *const ZrsCmdBuf) -> *const c_char {
    (*b).nice_name_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn zrs_cmdbuf_log_state(b: *const ZrsCmdBuf) {
    (*b).log_state()
}
