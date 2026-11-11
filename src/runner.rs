//! Core: runs a glibc binary through the real loader from the prefix.
//!
//! Instead of patchelf-ing the binary, we invoke `ld-linux-x86-64.so.2` as a
//! program with `--library-path` pointing at the prefix library directory.
//! This removes the need to modify the binary file.
//!
//! With path virtualization enabled we fork: the child installs the seccomp
//! filter and execs the loader, while the parent becomes a supervisor that
//! redirects `openat` to the `rootfs/` overlay.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use crate::seccomp;
use crate::supervisor;

pub struct LaunchSpec {
    pub loader: PathBuf,
    pub lib_dirs: Vec<PathBuf>,
    pub binary: PathBuf,
    pub argv0: String,
    pub args: Vec<String>,
    pub virt: bool,
    pub rootfs: Option<PathBuf>,
    pub verbose: bool,
}

impl LaunchSpec {
    /// Builds the loader argv:
    /// [loader, --library-path, <dirs>, --argv0, <argv0>, <binary>, <args...>]
    pub fn loader_argv(&self) -> Vec<String> {
        let libpath = self
            .lib_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let mut v = vec![
            self.loader.to_string_lossy().into_owned(),
            "--library-path".to_string(),
            libpath,
            "--argv0".to_string(),
            self.argv0.clone(),
            self.binary.to_string_lossy().into_owned(),
        ];
        v.extend(self.args.iter().cloned());
        v
    }
}

fn exec_loader(loader: &PathBuf, argv: &[String]) -> ! {
    let prog = CString::new(loader.as_os_str().as_bytes()).unwrap();
    let cargs: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_bytes()).unwrap())
        .collect();
    let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(ptr::null());
    unsafe {
        libc::execv(prog.as_ptr(), ptrs.as_ptr());
    }
    eprintln!(
        "glibc-run: loader execv failed: {}",
        std::io::Error::last_os_error()
    );
    std::process::exit(127);
}

/// Runs according to the spec. Does not return in the non-virtualized mode
/// (it replaces the process); with virtualization it returns the child's
/// exit code.
pub fn run(spec: &LaunchSpec) -> i32 {
    let argv = spec.loader_argv();

    if !spec.virt {
        exec_loader(&spec.loader, &argv);
    }

    let rootfs = match &spec.rootfs {
        Some(r) => r.clone(),
        None => exec_loader(&spec.loader, &argv), // no overlay = nothing to virtualize
    };

    // Socket pair to pass the listener descriptor from the child to the parent.
    let mut sv = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) } != 0 {
        eprintln!("glibc-run: socketpair failed — running without virtualization");
        exec_loader(&spec.loader, &argv);
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("glibc-run: fork failed — running without virtualization");
        exec_loader(&spec.loader, &argv);
    }

    if pid == 0 {
        // --- child ---
        unsafe {
            libc::close(sv[0]);
            // Without this an unprivileged process will not get the listener.
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        }
        let lfd = seccomp::install_listener();
        if lfd >= 0 {
            seccomp::send_fd(sv[1], lfd);
        } else {
            eprintln!(
                "glibc-run: failed to install the seccomp filter ({}) — no path virtualization",
                std::io::Error::last_os_error()
            );
        }
        unsafe {
            libc::close(sv[1]);
        }
        exec_loader(&spec.loader, &argv);
    }

    // --- parent (supervisor) ---
    unsafe {
        libc::close(sv[1]);
    }
    let lfd = seccomp::recv_fd(sv[0]);
    unsafe {
        libc::close(sv[0]);
    }
    if lfd >= 0 {
        supervisor::run(lfd, &rootfs, spec.verbose);
        unsafe {
            libc::close(lfd);
        }
    }

    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0);
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}
