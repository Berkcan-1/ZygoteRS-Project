//! Wire format of the unsolicited SIGCHLD message zygote sends to system_server.
//!
//! The *sender* (`sendSigChildStatus`) lives in the signal handler and stays in C++;
//! C++ static_asserts pin its struct layout to these constants.

pub const TYPE_SIGCHLD: u32 = 1;
pub const MSG_SIZE: usize = 16; // header{u32 type} + payload{i32 pid, u32 uid, i32 status}

/// Returns `[pid, uid, status]` for a well-formed SIGCHLD message, `None` otherwise.
/// Byte-wise decoding: the Java byte[] gives no alignment guarantee.
pub fn parse(bytes: &[u8]) -> Option<[i32; 3]> {
    if bytes.len() != MSG_SIZE {
        return None;
    }
    let word = |off: usize| [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
    if u32::from_ne_bytes(word(0)) != TYPE_SIGCHLD {
        return None;
    }
    Some([
        i32::from_ne_bytes(word(4)),
        u32::from_ne_bytes(word(8)) as i32,
        i32::from_ne_bytes(word(12)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ty: u32, pid: i32, uid: u32, status: i32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ty.to_ne_bytes());
        v.extend_from_slice(&pid.to_ne_bytes());
        v.extend_from_slice(&uid.to_ne_bytes());
        v.extend_from_slice(&status.to_ne_bytes());
        v
    }

    #[test]
    fn parses_valid() {
        assert_eq!(parse(&msg(1, 1234, 10123, 0x100)), Some([1234, 10123, 0x100]));
    }

    #[test]
    fn rejects_bad_type_and_length() {
        assert_eq!(parse(&msg(0, 1, 2, 3)), None);
        assert_eq!(parse(&msg(2, 1, 2, 3)), None);
        assert_eq!(parse(&msg(1, 1, 2, 3)[..15]), None);
        let mut long = msg(1, 1, 2, 3);
        long.push(0);
        assert_eq!(parse(&long), None);
    }
}
