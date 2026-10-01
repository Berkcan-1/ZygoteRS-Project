//! Port of `NativeCommandBuffer` (ZygoteCommandBuffer.cpp).
//!
//! This is the part of zygote that parses the wire protocol coming from system_server's
//! socket, i.e. the most "parser-shaped" code in the JNI layer, hence a good first target.
//!
//! Behaviour is kept byte-for-byte, with ONE deliberate change, marked `BEHAVIOUR CHANGE`:
//! `read_all_lines` on EOF / full buffer. The C++ loop
//!     while (mLinesLeft > 0) { readLine(fail_fn); }
//! ignored readLine()'s "no line" result, so a peer that disconnects mid-command made
//! zygote spin forever at 100% CPU. We return an error instead (same message / same
//! fatal path that `nativeNextArg` already uses for an incomplete command).

use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::{mem, ptr};

use crate::errbuf::{errno, strerror};
use crate::{log, props};

pub const MAX_COMMAND_BYTES: usize = 32768;
pub const NICE_NAME_BYTES: usize = 128;

const RUNTIME_ARGS: &[u8] = b"--runtime-args";
const INVOKE_WITH: &[u8] = b"--invoke-with";
const CHILD_ZYGOTE: &[u8] = b"--start-child-zygote";
const SETUID: &[u8] = b"--setuid=";
const SETGID: &[u8] = b"--setgid=";
const CAPABILITIES: &[u8] = b"--capabilities";
const NICE_NAME: &[u8] = b"--nice-name=";

/// Only one buffer may exist at a time (same invariant as `buffersAllocd` in C++).
static BUFFERS_ALLOCD: AtomicU32 = AtomicU32::new(0);

/// A buffer optionally bundled with a file descriptor from which we can fill it.
/// Does not own the fd. Lives in its own anonymous mmap, like the C++ version, so it is
/// page aligned and disappears with `munmap` instead of lingering on the malloc heap.
#[repr(C)]
pub struct CommandBuffer {
    end: u32,        // index of first empty byte in `buffer`
    next: u32,       // index of first char past the last line returned by read_line
    lines_left: i32, // lines in the current command not yet read
    fd: i32,         // -1 if none
    nice_name: [u8; NICE_NAME_BYTES], // always NUL terminated
    buffer: [u8; MAX_COMMAND_BYTES],
}

impl CommandBuffer {
    /// mmap a zero-filled buffer. Null on failure. Panics (=> abort) if one already exists.
    ///
    /// # Safety
    /// Caller must pair with [`CommandBuffer::free`] exactly once.
    pub unsafe fn alloc(fd: i32) -> *mut CommandBuffer {
        let prev = BUFFERS_ALLOCD.fetch_add(1, SeqCst);
        assert!(prev == 0, "ZygoteCommandBuffer: only one native buffer may exist at a time");
        let mem = libc::mmap(
            ptr::null_mut(),
            mem::size_of::<CommandBuffer>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
            -1,
            0,
        );
        if mem == libc::MAP_FAILED {
            BUFFERS_ALLOCD.fetch_sub(1, SeqCst);
            return ptr::null_mut();
        }
        let p = mem as *mut CommandBuffer;
        // Anonymous mappings are zero-filled: end = next = lines_left = 0, nice_name = "".
        (*p).fd = fd;
        p
    }

    /// # Safety
    /// `p` must come from [`CommandBuffer::alloc`] and not be used afterwards.
    pub unsafe fn free(p: *mut CommandBuffer) -> bool {
        let prev = BUFFERS_ALLOCD.fetch_sub(1, SeqCst);
        assert!(prev == 1, "ZygoteCommandBuffer: freeNativeBuffer without matching getNativeBuffer");
        libc::munmap(p as *mut libc::c_void, mem::size_of::<CommandBuffer>()) == 0
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn set_fd(&mut self, fd: i32) {
        self.fd = fd;
    }

    pub fn nice_name_ptr(&self) -> *const c_char {
        self.nice_name.as_ptr() as *const c_char
    }

    /// Pointer to the first byte of a line returned by `read_line` (valid until next mutation).
    pub fn line_ptr(&self, start: usize) -> *const c_char {
        self.buffer[start..].as_ptr() as *const c_char
    }

    pub fn reset(&mut self) {
        self.next = 0;
    }

    pub fn clear(&mut self) {
        // Don't bother zeroing `buffer`; it is unmapped in the child anyway.
        self.reset();
        self.nice_name[0] = 0;
        self.end = 0;
    }

    /// Reads the next line, refilling from `fd` as needed. Returns `(start, end)` where
    /// `end` indexes the '\n'. `Ok(None)` = EOF / buffer full (caller decides what that means).
    pub fn read_line(&mut self) -> Result<Option<(usize, usize)>, String> {
        let result = self.next as usize;
        loop {
            if self.next == self.end {
                if self.end as usize == MAX_COMMAND_BYTES {
                    return Ok(None);
                }
                if self.fd == -1 {
                    return Err("ZygoteCommandBuffer.readLine attempted to read from mFd -1".to_string());
                }
                let room = MAX_COMMAND_BYTES - self.end as usize;
                let nread = loop {
                    let n = unsafe {
                        libc::read(
                            self.fd,
                            self.buffer.as_mut_ptr().add(self.end as usize) as *mut libc::c_void,
                            room,
                        )
                    };
                    if n == -1 && errno() == libc::EINTR {
                        continue; // TEMP_FAILURE_RETRY
                    }
                    break n;
                };
                if nread <= 0 {
                    if nread == 0 {
                        return Ok(None);
                    }
                    return Err(format!("session socket read failed: {}", strerror(errno())));
                } else if nread as usize == room {
                    // Pessimistic by one character, but close enough (same as C++).
                    return Err("ZygoteCommandBuffer overflowed: command too long".to_string());
                }
                self.end += nread as u32;
            }
            // UTF-8 never contains '\n' inside a multibyte character.
            let (next, end) = (self.next as usize, self.end as usize);
            match self.buffer[next..end].iter().position(|&b| b == b'\n') {
                None => self.next = self.end,
                Some(i) => {
                    let nl = next + i;
                    self.next = (nl + 1) as u32;
                    self.lines_left -= 1;
                    if self.lines_left < 0 {
                        return Err(
                            "ZygoteCommandBuffer.readLine attempted to read past mEnd of command"
                                .to_string(),
                        );
                    }
                    return Ok(Some((result, nl)));
                }
            }
        }
    }

    /// Start a new command: returns the number of arguments (line count), 0 on EOF.
    pub fn get_count(&mut self) -> Result<i32, String> {
        self.lines_left = 1;
        let (s, e) = match self.read_line()? {
            None => return Ok(0),
            Some(p) => p,
        };
        let n = atol(&self.buffer[s..e]);
        if n <= 0 || n >= (MAX_COMMAND_BYTES / 2) as i64 {
            return Err(format!("Unreasonable argument count {}", n));
        }
        self.lines_left = n as i32;
        Ok(n as i32)
    }

    /// Make sure the current command is fully buffered, without reading past it.
    pub fn read_all_lines(&mut self) -> Result<(), String> {
        while self.lines_left > 0 {
            // BEHAVIOUR CHANGE: see module docs (C++ spun forever on EOF here).
            if self.read_line()?.is_none() {
                return Err("Incomplete zygote command".to_string());
            }
        }
        Ok(())
    }

    /// Insert a line (newline is added). Only for fd-less buffers.
    pub fn insert(&mut self, line: &[u8]) -> Result<(), String> {
        debug_assert!(self.fd == -1);
        let end = self.end as usize;
        if end + line.len() >= MAX_COMMAND_BYTES {
            return Err("ZygoteCommandBuffer.insert: command too long".to_string());
        }
        self.buffer[end..end + line.len()].copy_from_slice(line);
        self.buffer[end + line.len()] = b'\n';
        self.end += (line.len() + 1) as u32;
        Ok(())
    }

    /// Is the buffered command a simple fork command we may handle natively?
    /// Disallows wrapped children, child zygotes, anything mentioning capabilities, and
    /// uid < `min_uid`. Requires --runtime-args, --setuid=, --setgid= to be present.
    /// Side effect: fills `nice_name` when a --nice-name= argument is seen.
    pub fn is_simple_fork_command(&mut self, min_uid: i32) -> Result<bool, String> {
        if self.lines_left <= 0 || self.lines_left as usize >= MAX_COMMAND_BYTES / 2 {
            return Ok(false);
        }
        let (mut saw_setuid, mut saw_setgid, mut saw_runtime_args) = (false, false, false);

        while self.lines_left > 0 {
            let (s, e) = match self.read_line()? {
                None => return Ok(false),
                Some(p) => p,
            };
            let arg = &self.buffer[s..e];

            if arg == RUNTIME_ARGS {
                saw_runtime_args = true;
                continue;
            }
            if arg.starts_with(NICE_NAME) {
                let name = &arg[NICE_NAME.len()..];
                let n = name.len().min(NICE_NAME_BYTES - 1);
                self.nice_name[..n].copy_from_slice(&name[..n]);
                self.nice_name[n] = 0;
                if have_wrap_property(&self.nice_name) {
                    return Ok(false);
                }
                continue;
            }
            if arg == INVOKE_WITH {
                // Also removes the need for invoke-with security checks here.
                return Ok(false);
            }
            if arg == CHILD_ZYGOTE {
                return Ok(false);
            }
            if arg.starts_with(CAPABILITIES) {
                return Ok(false);
            }
            if arg.starts_with(SETUID) {
                let uid = digits_val(&arg[SETUID.len()..]);
                if uid < min_uid {
                    return Ok(false);
                }
                saw_setuid = true;
                continue;
            }
            if arg.starts_with(SETGID) {
                let gid = digits_val(&arg[SETGID.len()..]);
                if gid == -1 {
                    return Ok(false);
                }
                saw_setgid = true;
            }
            // ro.debuggable can be handled entirely in the child unless --invoke-with is
            // also specified, so it needs no check here.
        }
        Ok(saw_runtime_args && saw_setuid && saw_setgid)
    }

    pub fn log_state(&self) {
        let c0 = self.buffer[0] as char;
        let c1 = if self.buffer[1] == b'\n' { ' ' } else { self.buffer[1] as char };
        let name_len = self.nice_name.iter().position(|&b| b == 0).unwrap_or(NICE_NAME_BYTES);
        log::d(&format!(
            "mbuffer starts with {}{}, nice name is {}, mEnd = {}, mNext = {}, mLinesLeft = {}, mFd = {}",
            c0,
            c1,
            String::from_utf8_lossy(&self.nice_name[..name_len]),
            self.end,
            self.next,
            self.lines_left,
            self.fd
        ));
    }
}

/// `wrap.<nice-name>` system property present?
fn have_wrap_property(nice_name: &[u8; NICE_NAME_BYTES]) -> bool {
    let len = nice_name.iter().position(|&b| b == 0).unwrap_or(NICE_NAME_BYTES);
    let mut key = Vec::with_capacity(5 + len);
    key.extend_from_slice(b"wrap.");
    key.extend_from_slice(&nice_name[..len]);
    match CString::new(key) {
        Ok(c) => props::exists(&c),
        Err(_) => false,
    }
}

/// Picky atoi(): digits only, at most 6 of them. -1 on failure. (An empty string is 0,
/// exactly like the C++ loop.)
fn digits_val(s: &[u8]) -> i32 {
    if s.len() > 6 {
        return -1;
    }
    let mut r = 0i32;
    for &c in s {
        if !c.is_ascii_digit() {
            log::w("Argument failed integer format check");
            return -1;
        }
        r = 10 * r + (c - b'0') as i32;
    }
    r
}

/// atol() over a bounded slice (leading whitespace, optional sign, digits; saturating).
fn atol(s: &[u8]) -> i64 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut v: i64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((s[i] - b'0') as i64);
        i += 1;
    }
    if neg {
        -v
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(fd: i32) -> Box<CommandBuffer> {
        let mut b: Box<CommandBuffer> = unsafe { Box::new(mem::zeroed()) };
        b.fd = fd;
        b
    }

    /// Buffer whose fd yields `data`, then EOF.
    fn feed(data: &[u8]) -> Box<CommandBuffer> {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let n = unsafe { libc::write(fds[1], data.as_ptr() as *const libc::c_void, data.len()) };
        assert_eq!(n as usize, data.len());
        unsafe { libc::close(fds[1]) };
        boxed(fds[0])
    }

    fn line<'a>(b: &'a CommandBuffer, r: (usize, usize)) -> &'a [u8] {
        &b.buffer[r.0..r.1]
    }

    #[test]
    fn digits_and_atol() {
        assert_eq!(digits_val(b"10123"), 10123);
        assert_eq!(digits_val(b""), 0);
        assert_eq!(digits_val(b"1234567"), -1);
        assert_eq!(digits_val(b"12a"), -1);
        assert_eq!(digits_val(b"-1"), -1);
        assert_eq!(atol(b"  42"), 42);
        assert_eq!(atol(b"-7"), -7);
        assert_eq!(atol(b"abc"), 0);
        assert_eq!(atol(b"99999999999999999999999"), i64::MAX);
    }

    #[test]
    fn reads_lines_and_count() {
        let mut b = feed(b"2\nfoo\nbar\n");
        assert_eq!(b.get_count(), Ok(2));
        let l = b.read_line().unwrap().unwrap();
        assert_eq!(line(&b, l), b"foo");
        let l = b.read_line().unwrap().unwrap();
        assert_eq!(line(&b, l), b"bar");
    }

    #[test]
    fn count_bounds() {
        assert_eq!(feed(b"").get_count(), Ok(0)); // EOF
        assert!(feed(b"0\n").get_count().is_err());
        assert!(feed(b"-3\n").get_count().is_err());
        assert!(feed(b"16384\n").get_count().is_err());
        assert!(feed(b"abc\n").get_count().is_err());
    }

    #[test]
    fn simple_fork_accepted() {
        let mut b = feed(b"4\n--runtime-args\n--setuid=10100\n--setgid=10100\n--nice-name=app\n");
        assert_eq!(b.get_count(), Ok(4));
        assert_eq!(b.is_simple_fork_command(10000), Ok(true));
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(b.nice_name_ptr()) }.to_bytes(), b"app");
    }

    #[test]
    fn simple_fork_rejections() {
        let cases: &[(&[u8], i32)] = &[
            (b"3\n--runtime-args\n--setuid=1000\n--setgid=1000\n", 10000), // uid < min
            (b"3\n--runtime-args\n--setuid=10100\n--invoke-with\n", 10000),
            (b"3\n--runtime-args\n--setuid=10100\n--start-child-zygote\n", 10000),
            (b"3\n--runtime-args\n--setuid=10100\n--capabilities=1,1\n", 10000),
            (b"2\n--runtime-args\n--setuid=10100\n", 10000),  // no --setgid
            (b"2\n--setuid=10100\n--setgid=10100\n", 10000),   // no --runtime-args
            (b"3\n--runtime-args\n--setuid=10x00\n--setgid=1\n", 10000), // bad digits
            (b"3\n--runtime-args\n--setuid=10100\n--setgid=x\n", 10000),
        ];
        for (data, min_uid) in cases {
            let mut b = feed(data);
            assert!(b.get_count().unwrap() > 0);
            assert_eq!(b.is_simple_fork_command(*min_uid), Ok(false), "{:?}", String::from_utf8_lossy(data));
        }
    }

    #[test]
    fn eof_mid_command_is_an_error_not_a_spin() {
        let mut b = feed(b"3\n--runtime-args\n");
        assert_eq!(b.get_count(), Ok(3));
        assert_eq!(b.read_all_lines(), Err("Incomplete zygote command".to_string()));
    }

    #[test]
    fn overflow_is_reported() {
        // Exactly MAX_COMMAND_BYTES bytes, no complete command: the first read fills the whole
        // buffer, which the parser treats as "command too long" (same as the C++ check).
        let mut data = vec![b'7', b'\n'];
        data.extend(std::iter::repeat(b'a').take(MAX_COMMAND_BYTES - 2));
        assert_eq!(data.len(), MAX_COMMAND_BYTES);
        let mut b = feed(&data); // pipe capacity (64K) holds it in one write
        assert_eq!(
            b.get_count(),
            Err("ZygoteCommandBuffer overflowed: command too long".to_string())
        );
    }

    #[test]
    fn insert_and_clear() {
        let mut b = boxed(-1);
        b.insert(b"hello").unwrap();
        b.lines_left = 1;
        let l = b.read_line().unwrap().unwrap();
        assert_eq!(line(&b, l), b"hello");
        b.clear();
        assert_eq!((b.end, b.next), (0, 0));
        assert!(b.insert(&vec![b'x'; MAX_COMMAND_BYTES]).is_err());
    }

    // Only test that touches the process-global BUFFERS_ALLOCD.
    #[test]
    fn mmap_alloc_free_roundtrip() {
        unsafe {
            let p = CommandBuffer::alloc(-1);
            assert!(!p.is_null());
            assert_eq!((*p).end, 0);
            assert_eq!((*p).fd, -1);
            assert_eq!((*p).nice_name[0], 0);
            assert!(CommandBuffer::free(p));
            let p2 = CommandBuffer::alloc(5);
            assert!(!p2.is_null());
            assert!(CommandBuffer::free(p2));
        }
    }
}
