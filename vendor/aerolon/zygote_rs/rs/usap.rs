//! USAP (unspecialized app process) accounting table.
//!
//! Port of `UsapTableEntry` / `gUsapTable` / `gUsapPoolCount`.
//!
//! ASYNC-SIGNAL-SAFETY: `remove()` is called from the SIGCHLD handler. It therefore
//! only touches atomics and calls close(2): no allocation, no locks, no logging,
//! no panics (Soong builds device Rust with panic=abort; indexing is via iterators).
//!
//! Each slot packs `(pid: i32, read_pipe_fd: i32)` into one `AtomicU64`, which is the
//! same "always lock free, 8 bytes" property the C++ code static_asserted.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst, Ordering::Relaxed};

/// Mirror of ZygoteServer.USAP_POOL_SIZE_MAX_LIMIT.
pub const POOL_SIZE_MAX_LIMIT: usize = 100;

/// pid = -1, fd = -1 (same sentinel as `INVALID_ENTRY_VALUE` in the C++ code).
const INVALID: u64 = u64::MAX;

#[inline]
const fn pack(pid: i32, fd: i32) -> u64 {
    ((pid as u32 as u64) << 32) | (fd as u32 as u64)
}

#[inline]
const fn unpack(v: u64) -> (i32, i32) {
    ((v >> 32) as u32 as i32, v as u32 as i32)
}

// `const` item repeat works on old rustc; `[const { .. }; N]` would need 1.79+.
#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: AtomicU64 = AtomicU64::new(INVALID);
static TABLE: [AtomicU64; POOL_SIZE_MAX_LIMIT] = [EMPTY; POOL_SIZE_MAX_LIMIT];
static INSERT_INDEX: AtomicUsize = AtomicUsize::new(0);
static POOL_COUNT: AtomicU32 = AtomicU32::new(0);

/// Adds a USAP. Returns false if the table is full.
/// (The C++ original hit `__builtin_unreachable()` here, i.e. UB; we report instead.)
pub fn add(pid: i32, read_pipe_fd: i32) -> bool {
    let new = pack(pid, read_pipe_fd);
    let start = INSERT_INDEX.load(Relaxed) % POOL_SIZE_MAX_LIMIT;
    let mut i = start;
    loop {
        if TABLE[i].compare_exchange(INVALID, new, SeqCst, SeqCst).is_ok() {
            POOL_COUNT.fetch_add(1, SeqCst);
            // Start the next search right after where this one finished.
            INSERT_INDEX.store((i + 1) % POOL_SIZE_MAX_LIMIT, Relaxed);
            return true;
        }
        i = (i + 1) % POOL_SIZE_MAX_LIMIT;
        if i == start {
            return false;
        }
    }
}

/// Clears the entry for `pid` (closing its read fd) if present. SIGNAL-SAFE.
/// Returns true iff an entry was cleared by this call.
pub fn remove(pid: i32) -> bool {
    for slot in TABLE.iter() {
        if clear_for_pid(slot, pid) {
            POOL_COUNT.fetch_sub(1, SeqCst);
            return true;
        }
    }
    false
}

fn clear_for_pid(slot: &AtomicU64, pid: i32) -> bool {
    let cur = slot.load(SeqCst);
    // `cur != INVALID` is stricter than the C++ (which matched pid == -1 against empty
    // slots and would close(-1) / decrement the count); waitpid() never yields -1 here.
    if cur == INVALID {
        return false;
    }
    let (p, fd) = unpack(cur);
    if p != pid {
        return false;
    }
    // CAS outcomes, same as the C++ comment: success -> we own the close;
    // failure -> someone else already cleared (and maybe reused) the slot.
    if slot.compare_exchange(cur, INVALID, SeqCst, SeqCst).is_ok() {
        unsafe { libc::close(fd) };
        true
    } else {
        false
    }
}

/// Post-fork in the child: drop every entry (closing fds) and zero the count.
pub fn clear_all() {
    for slot in TABLE.iter() {
        let cur = slot.load(SeqCst);
        if cur != INVALID {
            let (_, fd) = unpack(cur);
            unsafe { libc::close(fd) };
            slot.store(INVALID, SeqCst);
        }
    }
    POOL_COUNT.store(0, SeqCst);
}

/// SIGTERM every pooled USAP, close its pipe and forget it.
pub fn empty_pool() {
    for slot in TABLE.iter() {
        let cur = slot.load(SeqCst);
        if cur != INVALID {
            let (pid, fd) = unpack(cur);
            unsafe {
                libc::kill(pid, libc::SIGTERM);
                // Clean up here so a newly created USAP is guaranteed a free slot even if
                // the SIGCHLD handler has not run yet.
                libc::close(fd);
            }
            slot.store(INVALID, SeqCst);
            POOL_COUNT.fetch_sub(1, SeqCst);
        }
    }
}

pub fn count() -> u32 {
    POOL_COUNT.load(SeqCst)
}

/// Copies the read-pipe fd of every live entry into `out`; returns how many were written.
pub fn read_fds(out: &mut [i32]) -> usize {
    let mut n = 0;
    for slot in TABLE.iter() {
        if n == out.len() {
            break;
        }
        let cur = slot.load(SeqCst);
        if cur != INVALID {
            out[n] = unpack(cur).1;
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe() -> (i32, i32) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    fn is_open(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    #[test]
    fn pack_roundtrip() {
        assert_eq!(unpack(pack(1234, 77)), (1234, 77));
        assert_eq!(unpack(pack(-1, -1)), (-1, -1));
        assert_eq!(pack(-1, -1), INVALID);
    }

    // One sequential test: the table is process-global, so parallel #[test]s would race.
    // (empty_pool() is not exercised: it kill()s real pids.)
    #[test]
    fn table_lifecycle() {
        clear_all();
        assert_eq!(count(), 0);

        let (r1, w1) = pipe();
        let (r2, w2) = pipe();
        assert!(add(1001, r1));
        assert!(add(1002, r2));
        assert_eq!(count(), 2);

        let mut out = [0i32; 8];
        let n = read_fds(&mut out);
        assert_eq!(n, 2);
        assert!(out[..n].contains(&r1) && out[..n].contains(&r2));
        let mut small = [0i32; 1];
        assert_eq!(read_fds(&mut small), 1);

        assert!(!remove(9999));
        assert!(!remove(-1)); // must not match empty slots
        assert_eq!(count(), 2);

        assert!(remove(1001));
        assert!(!is_open(r1));
        assert_eq!(count(), 1);
        assert!(!remove(1001)); // already cleared

        clear_all();
        assert!(!is_open(r2));
        assert_eq!(count(), 0);

        // Fill the table completely.
        let mut writers = vec![w1, w2];
        for i in 0..POOL_SIZE_MAX_LIMIT {
            let (r, w) = pipe();
            writers.push(w);
            assert!(add(2000 + i as i32, r));
        }
        assert_eq!(count() as usize, POOL_SIZE_MAX_LIMIT);
        let fd0_before = is_open(0);
        assert!(!add(5000, 0)); // full: reports instead of UB, and must not touch fd 0
        assert_eq!(is_open(0), fd0_before);
        clear_all();
        assert_eq!(count(), 0);
        for w in writers {
            unsafe { libc::close(w) };
        }
    }
}
