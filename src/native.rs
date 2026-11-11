//! --native mode: runs the glibc binary NATIVELY (without loader indirection),
//! so `/proc/self/exe` points at the binary, not at ld.so.
//!
//! Mechanism (rootless, no root on the real system):
//!  1. unshare(NEWUSER|NEWNS) — our own user and mount namespace.
//!  2. map uid/gid to root-in-ns (to be able to mount).
//!  3. bind-mount the glibc bundle onto /usr/lib. Since /lib64 → /usr/lib, the
//!     binary's hardcoded interpreter (/lib64/ld-linux-x86-64.so.2) hits our
//!     loader, and the glibc libraries shadow the musl ones. The change is
//!     visible only in this ns.
//!  4. execve the binary — the kernel loads it through our interpreter.
//!
//! This fixes applications that depend on /proc/self/exe (Electron/Chromium:
//! Claude, Epic), which cannot be started via an explicit ld.so invocation.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

/// Read-only overlay: `lower` shadows its names over `target`, the rest of
/// `target` stays visible. No upperdir (we never write anything).
fn mount_overlay(lower: &Path, target: &str) -> std::io::Result<()> {
    let data = CString::new(format!("lowerdir={}:{}", lower.display(), target)).unwrap();
    let src = CString::new("overlay").unwrap();
    let tgt = CString::new(target).unwrap();
    let r = unsafe {
        libc::mount(
            src.as_ptr(),
            tgt.as_ptr(),
            src.as_ptr(),
            0,
            data.as_ptr() as *const libc::c_void,
        )
    };
    if r != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Tmpfs matrix of a system directory: on a fresh tmpfs at `merged` it
/// recreates every entry of `real` — directories via a recursive bind,
/// symlinks by recreation, regular files via a file bind (after creating an
/// empty target). Also works under a parent with locked submounts, where an
/// overlay with `real` as lower and bind-norec both yield EINVAL.
fn synth_matrix(real: &Path, merged: &Path) -> Result<(), String> {
    let src = CString::new("tmpfs").unwrap();
    let tgt = CString::new(merged.as_os_str().as_bytes()).unwrap();
    let r = unsafe {
        libc::mount(src.as_ptr(), tgt.as_ptr(), src.as_ptr(), 0, ptr::null())
    };
    if r != 0 {
        return Err(format!("tmpfs: {}", std::io::Error::last_os_error()));
    }
    for ent in fs::read_dir(real).map_err(|e| format!("read {}: {e}", real.display()))? {
        let ent = ent.map_err(|e| e.to_string())?;
        let srcp = ent.path();
        let dstp = merged.join(ent.file_name());
        let md = fs::symlink_metadata(&srcp).map_err(|e| e.to_string())?;
        let ft = md.file_type();
        if ft.is_symlink() {
            let t = fs::read_link(&srcp).map_err(|e| e.to_string())?;
            std::os::unix::fs::symlink(&t, &dstp).map_err(|e| e.to_string())?;
        } else if ft.is_dir() {
            fs::create_dir(&dstp).map_err(|e| e.to_string())?;
            mount_bind(&srcp, &dstp.to_string_lossy())
                .map_err(|e| format!("bind {}: {e}", srcp.display()))?;
        } else if ft.is_file() {
            fs::write(&dstp, b"").map_err(|e| e.to_string())?;
            mount_bind(&srcp, &dstp.to_string_lossy())
                .map_err(|e| format!("bind {}: {e}", srcp.display()))?;
        }
        // sockets/fifos in /etc are skipped — irrelevant for applications
    }
    Ok(())
}

/// Bind WITHOUT recursion: a copy of the directory without its submounts —
/// useful as a clean lowerdir for overlayfs (submounts in the lower yield
/// EINVAL).
fn mount_bind_norec(src: &Path, target: &Path) -> std::io::Result<()> {
    let src_c = CString::new(src.as_os_str().as_bytes()).unwrap();
    let tgt_c = CString::new(target.as_os_str().as_bytes()).unwrap();
    let r = unsafe {
        libc::mount(
            src_c.as_ptr(),
            tgt_c.as_ptr(),
            ptr::null(),
            libc::MS_BIND,
            ptr::null(),
        )
    };
    if r != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn mount_bind(src: &Path, target: &str) -> std::io::Result<()> {
    let src_c = CString::new(src.as_os_str().as_bytes()).unwrap();
    let tgt_c = CString::new(target).unwrap();
    let r = unsafe {
        libc::mount(
            src_c.as_ptr(),
            tgt_c.as_ptr(),
            ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            ptr::null(),
        )
    };
    if r != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Runs the binary natively in a new namespace. Does not return on success.
pub fn run(
    bundle: &Path,
    rootfs: Option<&Path>,
    binary: &Path,
    argv0: &str,
    args: &[String],
    extra_ld: &[PathBuf],
    verbose: bool,
) -> i32 {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
        eprintln!(
            "glibc-run: unshare(NEWUSER|NEWNS) failed: {}\n\
             (unprivileged userns must be allowed; in a nested userns, \
             e.g. inside distrobox, this may be blocked — it works on a bare system)",
            std::io::Error::last_os_error()
        );
        return 1;
    }

    // Map uid->the same uid: the ns owner has CAP_SYS_ADMIN in the ns (mounting
    // works) while `id -u` stays unchanged — e.g. steam.sh refuses to run as root.
    let _ = fs::write("/proc/self/setgroups", "deny");
    if fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).is_err()
        || fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).is_err()
    {
        eprintln!(
            "glibc-run: writing uid/gid_map failed: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }

    // Make / private — so the binds do not leak into the parent ns (best-effort).
    unsafe {
        let root = CString::new("/").unwrap();
        libc::mount(
            ptr::null(),
            root.as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            ptr::null(),
        );
    }

    // Synthetic /usr (tmpfs matrix: a bind of every entry, symlinks recreated)
    // — deterministic also under a parent with locked submounts.
    // It adds: libexec from the runtime (hardcoded helper paths, e.g. WebKit)
    // and ".musl-lib" = a window onto the REAL /usr/lib from before the
    // overlay — thanks to it musl children (python/ctypes→libffi,
    // tools→libcrypto) get their libraries via /etc/ld-musl-x86_64.path even
    // though we cover /usr/lib.
    let libexec_src = bundle
        .parent()
        .map(|p| p.join("libexec"))
        .filter(|p| p.is_dir());
    let musl_window = (|| -> Result<(), String> {
        let merged = bundle.parent().unwrap().join(format!(".usr-merged-{}", std::process::id()));
        let _ = fs::create_dir_all(&merged);
        synth_matrix(Path::new("/usr"), &merged)?;
        // musl window: a second bind of the real /usr/lib (idempotent — when
        // nested, the matrix already copied the parent's .musl-lib entry)
        let ml = merged.join(".musl-lib");
        if let Err(e) = fs::create_dir(&ml) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.to_string());
            }
        }
        mount_bind(Path::new("/usr/lib"), &ml.to_string_lossy())
            .map_err(|e| format!("bind .musl-lib: {e}"))?;
        // libexec from the runtime
        if let Some(lx) = &libexec_src {
            let dst = merged.join("libexec");
            if dst.exists() {
                // the real libexec is already bound — add ours via overlay;
                // when nested, an overlay on an inherited bind can yield
                // ELOOP — then bind on top (our files take precedence)
                if mount_overlay(lx, &dst.to_string_lossy()).is_err() {
                    mount_bind(lx, &dst.to_string_lossy())
                        .map_err(|e| format!("bind libexec (fallback): {e}"))?;
                }
            } else {
                fs::create_dir(&dst).map_err(|e| e.to_string())?;
                mount_bind(lx, &dst.to_string_lossy())
                    .map_err(|e| format!("bind libexec: {e}"))?;
            }
        }
        mount_bind(&merged, "/usr").map_err(|e| format!("bind /usr: {e}"))
    })();
    let musl_window_ok = match musl_window {
        Ok(()) => {
            if verbose {
                eprintln!("glibc-run: [native] synthetic /usr (libexec + .musl-lib)");
            }
            true
        }
        Err(e) => {
            eprintln!("glibc-run: [native] synthetic /usr failed: {e}");
            false
        }
    };

    // Synthetic /etc: as above + an /etc/ld-musl-x86_64.path file pointing the
    // musl loader first at the .musl-lib window. Glibc does not read this file.
    if musl_window_ok {
        let etc = (|| -> Result<(), String> {
            let merged = bundle.parent().unwrap().join(format!(".etc-merged-{}", std::process::id()));
            let _ = fs::create_dir_all(&merged);
            synth_matrix(Path::new("/etc"), &merged)?;
            let pathfile = merged.join("ld-musl-x86_64.path");
            if !pathfile.exists() {
                fs::write(&pathfile, "/usr/.musl-lib\n/lib\n/usr/local/lib\n/usr/lib\n")
                    .map_err(|e| e.to_string())?;
            }
            // Files from <runtime>/etc/ shadow/extend the system /etc inside
            // the namespace (e.g. ld.so.cache for Steam's pressure-vessel).
            let extra = bundle.parent().unwrap().join("etc");
            if extra.is_dir() {
                for ent in fs::read_dir(&extra).map_err(|e| e.to_string())?.flatten() {
                    let dstp = merged.join(ent.file_name());
                    if !dstp.exists() {
                        fs::write(&dstp, b"").map_err(|e| e.to_string())?;
                    }
                    mount_bind(&ent.path(), &dstp.to_string_lossy())
                        .map_err(|e| format!("bind etc/{:?}: {e}", ent.file_name()))?;
                }
            }
            mount_bind(&merged, "/etc").map_err(|e| format!("bind /etc: {e}"))
        })();
        match etc {
            Ok(()) => {
                if verbose {
                    eprintln!("glibc-run: [native] synthetic /etc (ld-musl path)");
                }
            }
            Err(e) => eprintln!("glibc-run: [native] synthetic /etc failed: {e}"),
        }
    }

    // Glibc loaders -> /usr/lib. The overlay carries ONLY ld-linux* (the
    // interpreter must exist by path: /lib64/ld-linux-x86-64.so.2 →
    // /usr/lib). All remaining glibc libraries are resolved by
    // /etc/ld.so.cache (injected into the synthetic /etc) — a mechanism the
    // musl loader does NOT read. Thus musl processes see an untouched
    // /usr/lib (their libunistring/libffi/libcrypto), while glibc ones get
    // their versions via the cache. Zero soname collisions in either world.
    // (The old variant — the full runtime/lib as lower — remains as a
    //  fallback for runtimes that do not have a loaders/ directory yet.)
    let bundle = fs::canonicalize(bundle).unwrap_or_else(|_| bundle.to_path_buf());
    let loaders = bundle
        .parent()
        .map(|p| p.join("loaders"))
        .filter(|p| p.join("ld-linux-x86-64.so.2").exists());
    let overlay_lower: &Path = loaders.as_deref().unwrap_or(&bundle);
    if verbose && loaders.is_some() {
        eprintln!("glibc-run: [native] overlay = loaders only (the rest via ld.so.cache)");
    }
    let ovl_lower = overlay_lower.to_path_buf();
    let ovl_data =
        std::ffi::CString::new(format!("lowerdir={}:/usr/lib", ovl_lower.display())).unwrap();
    let ovl_src = std::ffi::CString::new("overlay").unwrap();
    let ovl_tgt = std::ffi::CString::new("/usr/lib").unwrap();
    let ovl = unsafe {
        libc::mount(
            ovl_src.as_ptr(),
            ovl_tgt.as_ptr(),
            ovl_src.as_ptr(),
            0,
            ovl_data.as_ptr() as *const libc::c_void,
        )
    };
    if ovl == 0 {
        if verbose {
            eprintln!("glibc-run: [native] overlay {} over /usr/lib", ovl_lower.display());
        }
    } else {
        let oe = std::io::Error::last_os_error();
        if let Err(e) = mount_bind(&ovl_lower, "/usr/lib") {
            eprintln!(
                "glibc-run: overlay failed ({oe}), bind {} -> /usr/lib failed too: {e}",
                ovl_lower.display()
            );
            return 1;
        }
        if verbose {
            eprintln!(
                "glibc-run: [native] overlay unavailable ({oe}) — full bind {} -> /usr/lib",
                ovl_lower.display()
            );
        }
    }

    // Vulkan: the runtime's ICDs are planted at the STANDARD path — the
    // Chromium sandbox (Brave etc.) lets the GPU process read
    // /usr/share/vulkan, but not $HOME/.glibc-run (hence "Failed to open
    // JSON" despite the file existing).
    let vk_src = bundle.parent().map(|p| p.join("vulkan"));
    if let Some(vk) = vk_src {
        let tgt = Path::new("/usr/share/vulkan/icd.d");
        if vk.is_dir() && tgt.is_dir() {
            match mount_bind(&vk, "/usr/share/vulkan/icd.d") {
                Ok(()) => {
                    if verbose {
                        eprintln!("glibc-run: [native] vulkan ICD: {} -> /usr/share/vulkan/icd.d", vk.display());
                    }
                }
                Err(e) => eprintln!("glibc-run: [native] vulkan ICD bind failed: {e}"),
            }
        }
    }

    // Rootfs overlay: bind every file/directory onto its existing system target.
    // (For targets that do not exist — e.g. /usr/libexec/webkit — the bind fails;
    //  that would need an overlay and is out of MVP scope; we log the skip.)
    if let Some(rf) = rootfs {
        bind_rootfs(rf, rf, verbose);
    }

    // LD_LIBRARY_PATH: in native mode ALL glibc libraries (64- and 32-bit)
    // are resolved by /etc/ld.so.cache — the glibc loaders read the cache and
    // pick the entry of the right ELF class themselves. glibc directories
    // MUST NOT be added here (neither lib nor lib32): the musl loader ALSO
    // reads LD_LIBRARY_PATH and — contrary to a common assumption — does NOT
    // skip a file of the wrong ELF class, it returns ENOEXEC on the first
    // name match. lib32 from Compat.i386 carries, among others, 32-bit
    // libreadline/libtinfo with the same sonames as the host musl ones; via
    // LD_LIBRARY_PATH they killed every musl child (e.g. bash:
    // "libreadline.so.8: Exec format error"). Only the user's explicit
    // directories from --lib remain — that is their deliberate decision.
    let mut ld = Vec::new();
    for e in extra_ld {
        ld.push(e.to_string_lossy().into_owned());
    }
    if ld.is_empty() {
        std::env::remove_var("LD_LIBRARY_PATH");
    } else {
        std::env::set_var("LD_LIBRARY_PATH", ld.join(":"));
    }

    // Marker for tools/scripts AND for idempotency (main.rs): we are inside a
    // glibc-run namespace; the value = the runtime root. A subsequent
    // glibc-run invocation with the same runtime (a glibc application
    // launching another wrapped program via PATH) then does not nest a
    // userns, it does a plain exec — the mounts inherited from us already
    // apply.
    match bundle.parent() {
        Some(rt) => std::env::set_var("GLIBC_RUN_NS", rt),
        None => std::env::set_var("GLIBC_RUN_NS", "1"),
    }

    // Fork: the child execs natively (its /proc/self/exe = the binary), the
    // parent stays and FORWARDS terminal signals (Ctrl+C etc.) to the child.
    // Some applications (Electron) can ignore SIGINT — the second signal is
    // escalated to SIGKILL so ^C ^C always works.
    let child = unsafe { libc::fork() };
    if child < 0 {
        eprintln!(
            "glibc-run: fork failed: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    if child == 0 {
        // --- child: native execve ---
        let prog = CString::new(binary.as_os_str().as_bytes()).unwrap();
        let mut cargv: Vec<CString> = Vec::with_capacity(args.len() + 1);
        cargv.push(CString::new(argv0).unwrap());
        for a in args {
            cargv.push(CString::new(a.as_bytes()).unwrap());
        }
        let mut ptrs: Vec<*const libc::c_char> = cargv.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        unsafe {
            libc::execv(prog.as_ptr(), ptrs.as_ptr());
        }
        eprintln!(
            "glibc-run: native execv failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(127);
    }

    // --- parent: signal forwarder + wait ---
    unsafe {
        CHILD_PID.store(child, std::sync::atomic::Ordering::SeqCst);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, forward_signal as usize as libc::sighandler_t);
        }
        // Ctrl+C from the terminal reaches the process group anyway (so the
        // child too); the handler catches the repeat and escalates.
        loop {
            let mut status: libc::c_int = 0;
            let r = libc::waitpid(child, &mut status, 0);
            if r == child {
                if libc::WIFEXITED(status) {
                    return libc::WEXITSTATUS(status);
                }
                if libc::WIFSIGNALED(status) {
                    return 128 + libc::WTERMSIG(status);
                }
                return 1;
            }
            if r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return 1;
            }
        }
    }
}

static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SIG_COUNT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid <= 0 {
        return;
    }
    let n = SIG_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    unsafe {
        if n == 0 {
            libc::kill(pid, sig);
        } else {
            // repeated signal: the application is unresponsive — kill the child's whole group
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Recursively bind-mounts files from the overlay onto their system paths.
fn bind_rootfs(root: &Path, dir: &Path, verbose: bool) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let sys = match path.strip_prefix(root) {
            Ok(rel) => PathBuf::from("/").join(rel),
            Err(_) => continue,
        };
        let is_dir = path.is_dir();
        if sys.exists() {
            // Target exists: file -> file bind; directory -> descend (so we do
            // not shadow the whole system directory).
            if is_dir {
                bind_rootfs(root, &path, verbose);
            } else if let Err(e) = mount_bind(&path, &sys.to_string_lossy()) {
                if verbose {
                    eprintln!("glibc-run: [native] rootfs bind {} skipped: {e}", sys.display());
                }
            } else if verbose {
                eprintln!("glibc-run: [native] rootfs {} -> {}", path.display(), sys.display());
            }
        } else if verbose {
            eprintln!(
                "glibc-run: [native] target {} does not exist — skipped (needs an overlay)",
                sys.display()
            );
        }
    }
}
