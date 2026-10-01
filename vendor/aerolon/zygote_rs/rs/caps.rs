//! Capability / credential helpers used by SpecializeCommon().
//!
//! 1:1 ports of: CalculateCapabilities, GetEffectiveCapabilityMask, EnableKeepCapabilities,
//! DropCapabilitiesBoundingSet, SetInheritable, SetCapabilities, SetGids, SetRLimits.
//! The ORDER in which SpecializeCommon calls them is unchanged (it stays in C++), so the
//! SELinux / seccomp / uid-transition sequencing is untouched.
//!
//! Every prctl() argument is passed as c_ulong: prctl is variadic and the kernel reads
//! full registers, a bare `0` literal would be passed as a 32-bit int on AArch64.

use std::os::raw::c_ulong;

use crate::errbuf::{errno, strerror};
use crate::log;

// Values from <private/android_filesystem_config.h>, <linux/capability.h>.
// C++ glue static_asserts every one of these against the real headers.
pub const AID_USER_OFFSET: u32 = 100_000;
pub const AID_BLUETOOTH: u32 = 1002;
pub const AID_NETWORK_STACK: u32 = 1073;
pub const AID_WAKELOCK: u32 = 3010;

pub const CAP_SETGID: u32 = 6;
pub const CAP_SETUID: u32 = 7;
pub const CAP_SETPCAP: u32 = 8;
pub const CAP_NET_BIND_SERVICE: u32 = 10;
pub const CAP_NET_BROADCAST: u32 = 11;
pub const CAP_NET_ADMIN: u32 = 12;
pub const CAP_NET_RAW: u32 = 13;
pub const CAP_SYS_NICE: u32 = 23;
pub const CAP_WAKE_ALARM: u32 = 35;
pub const CAP_BLOCK_SUSPEND: u32 = 36;

const PR_SET_KEEPCAPS: i32 = 8;
const PR_CAPBSET_READ: i32 = 23;
const PR_CAPBSET_DROP: i32 = 24;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const fn bit(cap: u32) -> u64 {
    1u64 << cap
}

fn capget(data: &mut [CapData; 2]) -> Result<(), String> {
    let mut hdr = CapHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let rc = unsafe { libc::syscall(libc::SYS_capget, &mut hdr as *mut CapHeader, data.as_mut_ptr()) };
    if rc == -1 {
        Err(format!("capget failed: {}", strerror(errno())))
    } else {
        Ok(())
    }
}

fn capset(data: &[CapData; 2]) -> Result<(), libc::c_int> {
    let mut hdr = CapHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let rc = unsafe { libc::syscall(libc::SYS_capset, &mut hdr as *mut CapHeader, data.as_ptr()) };
    if rc == -1 {
        Err(errno())
    } else {
        Ok(())
    }
}

/// Effective capability set of the calling process (containers may lack some caps).
pub fn effective_mask() -> Result<u64, String> {
    let mut data = [CapData::default(); 2];
    capget(&mut data)?;
    Ok((data[0].effective as u64) | ((data[1].effective as u64) << 32))
}

/// Pure capability policy. `effective_mask` is passed in so this is unit-testable.
pub fn calculate(
    uid: i32,
    gid: i32,
    gids: Option<&[i32]>,
    is_child_zygote: bool,
    effective_mask: u64,
) -> u64 {
    let mut caps = 0u64;
    let app_id = (uid as u32) % AID_USER_OFFSET;

    // Bluetooth: WAKE_ALARM, NET_ADMIN, NET_RAW, NET_BIND_SERVICE (DHCP), SYS_NICE (audio RT).
    if app_id == AID_BLUETOOTH {
        caps |= bit(CAP_WAKE_ALARM)
            | bit(CAP_NET_ADMIN)
            | bit(CAP_NET_RAW)
            | bit(CAP_NET_BIND_SERVICE)
            | bit(CAP_SYS_NICE);
    }

    if app_id == AID_NETWORK_STACK {
        caps |= bit(CAP_NET_ADMIN)
            | bit(CAP_NET_BROADCAST)
            | bit(CAP_NET_BIND_SERVICE)
            | bit(CAP_NET_RAW);
    }

    // CAP_BLOCK_SUSPEND for members of GID "wakelock".
    let wakelock = AID_WAKELOCK as i32;
    if gid == wakelock || gids.map_or(false, |g| g.contains(&wakelock)) {
        caps |= bit(CAP_BLOCK_SUSPEND);
    }

    // Child zygotes may change uid/gid/caps of their children.
    if is_child_zygote {
        caps |= bit(CAP_SETUID) | bit(CAP_SETGID) | bit(CAP_SETPCAP);
    }

    caps & effective_mask
}

pub fn enable_keep_capabilities() -> Result<(), String> {
    let rc = unsafe { libc::prctl(PR_SET_KEEPCAPS, 1 as c_ulong, 0 as c_ulong, 0 as c_ulong, 0 as c_ulong) };
    if rc == -1 {
        return Err(format!("prctl(PR_SET_KEEPCAPS) failed: {}", strerror(errno())));
    }
    Ok(())
}

pub fn drop_bounding_set() -> Result<(), String> {
    let mut i: c_ulong = 0;
    while unsafe { libc::prctl(PR_CAPBSET_READ, i, 0 as c_ulong, 0 as c_ulong, 0 as c_ulong) } >= 0 {
        if unsafe { libc::prctl(PR_CAPBSET_DROP, i, 0 as c_ulong, 0 as c_ulong, 0 as c_ulong) } == -1 {
            let e = errno();
            if e == libc::EINVAL {
                log::e(
                    "prctl(PR_CAPBSET_DROP) failed with EINVAL. Please verify \
                     your kernel is compiled with file capabilities support",
                );
            } else {
                return Err(format!("prctl(PR_CAPBSET_DROP, {}) failed: {}", i, strerror(e)));
            }
        }
        i += 1;
    }
    Ok(())
}

pub fn set_inheritable(inheritable: u64) -> Result<(), String> {
    let mut data = [CapData::default(); 2];
    capget(&mut data)?;
    data[0].inheritable = inheritable as u32;
    data[1].inheritable = (inheritable >> 32) as u32;
    capset(&data).map_err(|e| format!("capset(inh={:x}) failed: {}", inheritable, strerror(e)))
}

pub fn set_capabilities(permitted: u64, effective: u64, inheritable: u64) -> Result<(), String> {
    let data = [
        CapData { effective: effective as u32, permitted: permitted as u32, inheritable: inheritable as u32 },
        CapData {
            effective: (effective >> 32) as u32,
            permitted: (permitted >> 32) as u32,
            inheritable: (inheritable >> 32) as u32,
        },
    ];
    capset(&data).map_err(|e| {
        format!(
            "capset(perm={:x}, eff={:x}, inh={:x}) failed: {}",
            permitted,
            effective,
            inheritable,
            strerror(e)
        )
    })
}

/// `gids == None` models a null Java array.
pub fn set_gids(gids: Option<&[i32]>, is_child_zygote: bool) -> Result<(), String> {
    match gids {
        None => {
            // Child zygotes (webview / app zygote) must drop the parent's supplementary groups.
            if is_child_zygote && unsafe { libc::setgroups(0, std::ptr::null()) } == -1 {
                return Err("Failed to remove supplementary groups for child zygote".to_string());
            }
            Ok(())
        }
        Some(g) => {
            let rc = unsafe { libc::setgroups(g.len() as _, g.as_ptr() as *const libc::gid_t) };
            if rc == -1 {
                return Err(format!("setgroups failed: {}, gids.size={}", strerror(errno()), g.len()));
            }
            Ok(())
        }
    }
}

/// `triples` is a flat `[resource, rlim_cur, rlim_max, ...]` array.
/// Negative values sign-extend (-1 == RLIM_INFINITY) exactly like the C++ int -> rlim_t conversion.
pub fn set_rlimits(triples: &[i32]) -> Result<(), String> {
    if triples.len() % 3 != 0 {
        return Err("rlimits array must have a second dimension of size 3".to_string());
    }
    for t in triples.chunks_exact(3) {
        let rlim = libc::rlimit { rlim_cur: t[1] as libc::rlim_t, rlim_max: t[2] as libc::rlim_t };
        if unsafe { libc::setrlimit(t[0] as _, &rlim) } == -1 {
            return Err(format!(
                "setrlimit({}, {{{}, {}}}) failed",
                t[0], rlim.rlim_cur as i64, rlim.rlim_max as i64
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: u64 = u64::MAX;

    #[test]
    fn plain_app_gets_nothing() {
        assert_eq!(calculate(10123, 10123, None, false, ALL), 0);
    }

    #[test]
    fn bluetooth_any_user() {
        let want = bit(CAP_WAKE_ALARM) | bit(CAP_NET_ADMIN) | bit(CAP_NET_RAW)
            | bit(CAP_NET_BIND_SERVICE) | bit(CAP_SYS_NICE);
        assert_eq!(calculate(1002, 1002, None, false, ALL), want);
        assert_eq!(calculate(101_002, 1002, None, false, ALL), want); // user 1
    }

    #[test]
    fn network_stack() {
        let want = bit(CAP_NET_ADMIN) | bit(CAP_NET_BROADCAST) | bit(CAP_NET_BIND_SERVICE) | bit(CAP_NET_RAW);
        assert_eq!(calculate(1073, 1073, None, false, ALL), want);
    }

    #[test]
    fn wakelock_via_gid_or_gids() {
        assert_eq!(calculate(10001, 3010, None, false, ALL), bit(CAP_BLOCK_SUSPEND));
        assert_eq!(calculate(10001, 10001, Some(&[1, 3010]), false, ALL), bit(CAP_BLOCK_SUSPEND));
        assert_eq!(calculate(10001, 10001, Some(&[1, 2]), false, ALL), 0);
        assert_eq!(calculate(10001, 10001, Some(&[]), false, ALL), 0);
    }

    #[test]
    fn child_zygote() {
        assert_eq!(
            calculate(10001, 10001, None, true, ALL),
            bit(CAP_SETUID) | bit(CAP_SETGID) | bit(CAP_SETPCAP)
        );
    }

    #[test]
    fn mask_filters_missing_caps() {
        assert_eq!(calculate(1002, 1002, None, false, 0), 0);
        assert_eq!(calculate(1002, 1002, None, false, bit(CAP_NET_RAW)), bit(CAP_NET_RAW));
    }

    #[test]
    fn rlimit_shape_is_checked() {
        assert!(set_rlimits(&[1, 2]).is_err());
    }
}
