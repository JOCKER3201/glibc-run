//! Warstwa seccomp user-notify.
//!
//! Instalujemy filtr BPF, który na syscall `openat` zwraca SECCOMP_RET_USER_NOTIF,
//! a wszystko inne przepuszcza. Jądro oddaje nam deskryptor listenera; przez niego
//! superwizor (proces-rodzic) odbiera powiadomienia i może przekierować otwarcie
//! pliku, wstrzykując własny deskryptor (ADDFD z flagą SEND).
//!
//! `openat` pokrywa też `open()` z glibc (na x86_64 glibc woła openat). To wystarcza,
//! by przesłaniać zaszyte w kodzie ścieżki bibliotek, configów i zasobów.
//!
//! Ograniczenie konstrukcyjne: seccomp-notify NIE potrafi przepisać argumentu
//! ścieżki w `execve` — stąd ścieżki helperów typu /usr/libexec trzeba rozwiązać
//! inaczej (patrz README, sekcja o execve).

use std::os::unix::io::RawFd;

// Stałe seccomp/BPF (nie wszystkie są w crate libc).
const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const NR_OPENAT: u32 = 257;

// Klasy instrukcji BPF.
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

/// Instaluje filtr i zwraca deskryptor listenera (lub -1 przy błędzie).
/// Wywoływać w dziecku, PO ustawieniu PR_SET_NO_NEW_PRIVS.
pub fn install_listener() -> RawFd {
    // Układ (indeksy instrukcji):
    //   0: załaduj arch (offset 4 w seccomp_data)
    //   1: jeśli arch != x86_64 -> skok do ALLOW (idx 5)
    //   2: załaduj nr syscalla (offset 0)
    //   3: jeśli nr != openat -> skok do ALLOW (idx 5)
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

/// Wysyła deskryptor przez gniazdo (SCM_RIGHTS).
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

/// Odbiera deskryptor z gniazda (SCM_RIGHTS). Zwraca -1 przy braku.
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
