//! The seccomp user-notify layer.
//!
//! We install a BPF filter that returns SECCOMP_RET_USER_NOTIF for the `openat`
//! syscall and lets everything else through. The kernel hands us a listener
//! descriptor; through it the supervisor (the parent process) receives
//! notifications and can redirect a file open by injecting its own descriptor
//! (ADDFD with the SEND flag).
//!
//! `openat` also covers glibc's `open()` (on x86_64 glibc calls openat). That is
//! enough to shadow library, config and resource paths hardcoded in the code.
//!
//! Design limitation: seccomp-notify canNOT rewrite the path argument of
//! `execve` — so helper paths like /usr/libexec have to be solved differently
//! (see README, the section on execve).

use std::os::unix::io::RawFd;

// seccomp/BPF constants (not all of them are in the libc crate).
const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const NR_OPENAT: u32 = 257;

// BPF instruction classes.
const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Installs the filter and returns the listener descriptor (or -1 on error).
/// Call in the child, AFTER setting PR_SET_NO_NEW_PRIVS.
pub fn install_listener() -> RawFd {
    // Layout (instruction indices):
    //   0: load arch (offset 4 in seccomp_data)
    //   1: if arch != x86_64 -> jump to ALLOW (idx 5)
    //   2: load syscall nr (offset 0)
    //   3: if nr != openat -> jump to ALLOW (idx 5)
    //   4: RET USER_NOTIF
    //   5: RET ALLOW
    let filter = [
        stmt(BPF_LD | BPF_W | BPF_ABS, 4),
        jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 0, 3),
        stmt(BPF_LD | BPF_W | BPF_ABS, 0),
        jump(BPF_JMP | BPF_JEQ | BPF_K, NR_OPENAT, 0, 1),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    ];
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog as *const libc::sock_fprog,
        )
    };
    fd as RawFd
}

/// Sends a descriptor over a socket (SCM_RIGHTS).
pub fn send_fd(sock: RawFd, fd: RawFd) -> bool {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg_buf = [0u8; 32]; // >= CMSG_SPACE(sizeof(int))
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_buf.len() as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
        *data = fd;
        msg.msg_controllen = (*cmsg).cmsg_len as _;
        libc::sendmsg(sock, &msg, 0) >= 0
    }
}

/// Receives a descriptor from a socket (SCM_RIGHTS). Returns -1 if absent.
pub fn recv_fd(sock: RawFd) -> RawFd {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg_buf = [0u8; 32];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_buf.len() as _;
    unsafe {
        let n = libc::recvmsg(sock, &mut msg, 0);
        if n < 0 {
            return -1;
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return -1;
        }
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return -1;
        }
        let data = libc::CMSG_DATA(cmsg) as *const libc::c_int;
        *data
    }
}
