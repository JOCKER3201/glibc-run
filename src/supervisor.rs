//! Superwizor: pętla obsługi powiadomień seccomp user-notify.
//!
//! Dla każdego `openat` czyta ścieżkę z pamięci procesu-celu. Jeśli ścieżka jest
//! absolutna i istnieje jej odpowiednik w nakładce `rootfs/`, otwiera ten plik u
//! siebie i wstrzykuje deskryptor do celu (ADDFD z flagą SEND — atomowo instaluje
//! fd i ustawia go jako wynik syscalla). W przeciwnym razie przepuszcza syscall
//! natywnie (flaga CONTINUE).

use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

// --- struktury ABI seccomp-notify (repr C) ---

#[repr(C)]
#[derive(Clone, Copy)]
struct SeccompData {
    nr: i32,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

#[repr(C)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

#[repr(C)]
struct SeccompNotifAddfd {
    id: u64,
    flags: u32,
    srcfd: u32,
    newfd: u32,
    newfd_flags: u32,
}

const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
const SECCOMP_ADDFD_FLAG_SEND: u32 = 2;

// Numery ioctl liczone jak makra _IOC z jądra (asm-generic).
const fn ioc(dir: u64, typ: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (typ << 8) | nr
}
const MAGIC: u64 = b'!' as u64;
const NOTIF_RECV: u64 = ioc(3, MAGIC, 0, 80); // _IOWR, sizeof(SeccompNotif)=80
const NOTIF_SEND: u64 = ioc(3, MAGIC, 1, 24); // _IOWR, sizeof(SeccompNotifResp)=24
const NOTIF_ID_VALID: u64 = ioc(1, MAGIC, 2, 8); // _IOW, u64
const NOTIF_ADDFD: u64 = ioc(1, MAGIC, 3, 24); // _IOW, sizeof(SeccompNotifAddfd)=24

/// Czyta ścieżkę (C-string) z pamięci procesu `pid` spod adresu `addr`.
fn read_path(pid: i32, addr: u64) -> Option<String> {
    let mut buf = [0u8; 4096];
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if n <= 0 {
        return None;
    }
    let n = n as usize;
    let end = buf[..n].iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Mapuje absolutną ścieżkę systemową na plik w nakładce, jeśli taki istnieje.
fn map_path(rootfs: &Path, p: &str) -> Option<PathBuf> {
    if !p.starts_with('/') {
        return None;
    }
    let cand = rootfs.join(p.trim_start_matches('/'));
    if cand.exists() {
        Some(cand)
    } else {
        None
    }
}

fn send_continue(lfd: RawFd, id: u64) {
    let resp = SeccompNotifResp {
        id,
        val: 0,
        error: 0,
        flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
    };
    unsafe {
        libc::ioctl(lfd, NOTIF_SEND as _, &resp as *const SeccompNotifResp);
    }
}

fn send_errno(lfd: RawFd, id: u64, errno: i32) {
    let resp = SeccompNotifResp {
        id,
        val: 0,
        error: -errno,
        flags: 0,
    };
    unsafe {
        libc::ioctl(lfd, NOTIF_SEND as _, &resp as *const SeccompNotifResp);
    }
}

/// Główna pętla. Zwraca, gdy listener przestaje istnieć (całe drzewo celu padło).
pub fn run(lfd: RawFd, rootfs: &Path, verbose: bool) {
    loop {
        let mut req: SeccompNotif = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::ioctl(lfd, NOTIF_RECV as _, &mut req as *mut SeccompNotif) };
        if r != 0 {
            // ENOENT/EINTR/itp. — zwykle znaczy, że cel zakończył pracę.
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }

        // openat(dirfd=args[0], pathname=args[1], flags=args[2], mode=args[3])
        let path = read_path(req.pid as i32, req.data.args[1]);
        let newpath = path.as_deref().and_then(|p| map_path(rootfs, p));

        let newpath = match newpath {
            Some(np) => np,
            None => {
                send_continue(lfd, req.id);
                continue;
            }
        };

        // Upewnij się, że powiadomienie jest wciąż aktualne, zanim działamy.
        let valid = unsafe { libc::ioctl(lfd, NOTIF_ID_VALID as _, &req.id as *const u64) };
        if valid != 0 {
            continue; // nieaktualne — cel już odwołał syscall
        }

        let flags = req.data.args[2] as libc::c_int;
        let mode = req.data.args[3] as libc::c_uint;
        let cpath = match std::ffi::CString::new(newpath.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => {
                send_continue(lfd, req.id);
                continue;
            }
        };
        let sfd = unsafe { libc::open(cpath.as_ptr(), flags, mode) };
        if sfd < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
            send_errno(lfd, req.id, errno);
            continue;
        }

        let addfd = SeccompNotifAddfd {
            id: req.id,
            flags: SECCOMP_ADDFD_FLAG_SEND,
            srcfd: sfd as u32,
            newfd: 0,
            newfd_flags: (flags & libc::O_CLOEXEC) as u32,
        };
        let newfd = unsafe { libc::ioctl(lfd, NOTIF_ADDFD as _, &addfd as *const SeccompNotifAddfd) };
        unsafe {
            libc::close(sfd);
        }
        if newfd < 0 {
            // ADDFD nie przeszło (np. cel zniknął) — spróbuj przepuścić natywnie.
            send_continue(lfd, req.id);
        } else if verbose {
            if let Some(p) = path {
                eprintln!("glibc-run: przekierowano {} -> {}", p, newpath.display());
            }
        }
        // Przy sukcesie ADDFD-SEND jądro już wysłało odpowiedź; nie wołamy NOTIF_SEND.
    }
}
