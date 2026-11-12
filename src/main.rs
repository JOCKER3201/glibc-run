//! glibc-run — runs glibc binaries on a musl system.
//!
//! Like wine for Windows applications, but instead of reimplementing glibc it
//! carries a real glibc (prefix) and runs the binary through its loader. An
//! optional seccomp-notify layer redirects hardcoded paths to an overlay.

mod elf;
mod native;
mod runner;
mod seccomp;
mod supervisor;

use std::path::{Path, PathBuf};
use std::process::exit;

use runner::LaunchSpec;

fn usage() -> ! {
    eprint!(
        "glibc-run — runs glibc binaries on a musl system (like wine, but carries the real glibc)\n\
\n\
Usage: glibc-run <binary|script> [arguments...]\n\
\n\
With no options: runtime from ~/.glibc-run/runtime, native mode (userns+overlay),\n\
environment (GPU/glvnd, gdk-pixbuf, gsettings, gio, Vulkan, GNU tools) set up\n\
automatically from the runtime contents — only variables NOT yet set.\n\
HOME and user data stay UNTOUCHED (the single usual home).\n\
Inside an already running glibc-run namespace with the same runtime it does not\n\
nest a userns — it does a plain exec (marker: GLIBC_RUN_NS=<runtime root>).\n\
\n\
Options (advanced):\n\
  --prefix <dir>   Alternative runtime directory (default ~/.glibc-run/runtime\n\
                   or $GLIBC_RUN_PREFIX); expects <dir>/lib, optionally\n\
                   lib/lib32, tools, toolslibs, vulkan, rootfs\n\
  --lib <dir>      Extra library directory (may be repeated)\n\
  --rootfs <dir>   Path overlay (default <prefix>/rootfs)\n\
  --loader <file>  Explicit ld-linux path\n\
  --no-native      Loader mode instead of native (ld.so + --library-path;\n\
                   /proc/self/exe then points at the loader — breaks Electron/CEF)\n\
  --no-virt        In loader mode: no path virtualization (seccomp)\n\
  --no-env         Do not set up the automatic environment\n\
  --host <cmd...>  Run a command THE SYSTEM WAY: clears the glibc-run\n\
                   environment (PATH/tools, GIO/GDK/GL/WebKit, LD_LIBRARY_PATH);\n\
                   for use from inside the namespace (a terminal in an app)\n\
  --print          Print the launch plan and exit\n\
  -v, --verbose    Diagnostic logs on stderr\n\
  -h, --help       This help\n"
    );
    exit(2);
}

/// Sets a variable only if the user has not set it themselves.
fn setenv_default(key: &str, val: &str) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, val);
    }
}

/// In-namespace directory for helper binaries that glibc tooling must be able
/// to find through PATH under /usr. Native mode overlays `<runtime>/libexec`
/// on /usr/libexec, so `<runtime>/libexec/glibc-run` shows up at this path.
const NS_HELPER_BIN_DIR: &str = "/usr/libexec/glibc-run";

/// Automatic environment based on the runtime contents. Never touches HOME
/// or XDG_* — user data stays the same as usual.
fn auto_env(prefix: &Path, libdir: &Path, native: bool, verbose: bool) {
    // GNU tools (getopt/tar/...) ahead of the musl BSD-style ones — only prepends to PATH.
    let tools = prefix.join("tools");
    if tools.is_dir() {
        let old = std::env::var("PATH").unwrap_or_default();
        let t = tools.to_string_lossy();
        if !old.split(':').any(|p| p == t) {
            std::env::set_var("PATH", format!("{t}:{old}"));
        }
    }
    // Helpers for nested containers. pressure-vessel (Steam Linux Runtime)
    // rebuilds the container's ld.so.cache with the provider's `ldconfig`,
    // which it looks up through PATH and only accepts from a path under /usr.
    // A musl host ships a no-op stub there, so the container ends up without
    // a cache and breaks as soon as something drops LD_LIBRARY_PATH (SteamVR
    // does when it starts vrcompositor). The overlay only exists in native
    // mode, so the entry would point nowhere otherwise.
    if native && prefix.join("libexec/glibc-run").is_dir() {
        let old = std::env::var("PATH").unwrap_or_default();
        if !old.split(':').any(|p| p == NS_HELPER_BIN_DIR) {
            std::env::set_var("PATH", format!("{NS_HELPER_BIN_DIR}:{old}"));
        }
    }
    let set_if = |key: &str, p: PathBuf| {
        if p.exists() {
            setenv_default(key, &p.to_string_lossy());
        }
    };
    set_if("GDK_PIXBUF_MODULE_FILE", libdir.join("gdk-pixbuf/loaders.cache"));
    set_if("GDK_PIXBUF_MODULEDIR", libdir.join("gdk-pixbuf/loaders"));
    set_if("GSETTINGS_SCHEMA_DIR", libdir.join("glib-2.0/schemas"));
    set_if("GIO_MODULE_DIR", libdir.join("gio/modules"));
    // NVIDIA GPU stack — only when the module is in the kernel AND the
    // userspace version from the runtime matches the module version. On a
    // mismatch, forcing the vendor yields dead EGL (ANGLE: "Failed to get
    // system egl display"); a clean fallback to software rendering is better.
    let kernel_nv = std::fs::read_to_string("/proc/driver/nvidia/version")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .find(|w| w.contains('.') && w.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .map(|w| w.to_string())
        });
    let runtime_nv = std::fs::read_dir(libdir).ok().and_then(|rd| {
        rd.flatten().find_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("libGLX_nvidia.so.")
                .filter(|v| v.len() > 2)
                .map(|v| v.to_string())
        })
    });
    let nvidia_ok = match (&kernel_nv, &runtime_nv) {
        (Some(k), Some(r)) if k == r => true,
        (Some(k), Some(r)) => {
            eprintln!(
                "glibc-run: NVIDIA in kernel {k}, in runtime {r} — versions differ; \
                 software rendering (refresh the runtime with matching userspace)."
            );
            false
        }
        _ => false,
    };
    if nvidia_ok {
        set_if(
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            libdir.join("egl_vendor.d/10_nvidia.json"),
        );
        set_if(
            "__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS",
            libdir.join("egl_external"),
        );
        setenv_default("__GLX_VENDOR_LIBRARY_NAME", "nvidia");
        if libdir.join("gbm/nvidia-drm_gbm.so").exists() {
            setenv_default("GBM_BACKEND", "nvidia-drm");
            setenv_default("GBM_BACKENDS_PATH", &libdir.join("gbm").to_string_lossy());
        }
    } else if libdir.join("libGLX_mesa.so.0").exists() {
        // Universal fallback: Mesa is independent of the kernel version (stable
        // DRM ABI) — AMD/Intel get hardware acceleration, the rest llvmpipe.
        setenv_default("__GLX_VENDOR_LIBRARY_NAME", "mesa");
        set_if(
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            libdir.join("egl_vendor.d/50_mesa.json"),
        );
        set_if("LIBGL_DRIVERS_PATH", libdir.join("dri"));
    }
    // Vulkan: the loader itself skips ICDs whose driver does not respond —
    // all of them (nvidia + mesa radv/anv/lvp) land in native mode via a
    // bind on /usr/share/vulkan/icd.d (a path allowed by the Chromium
    // sandbox). The variable is set only as a FALLBACK, when that directory
    // does not exist on the system (the bind has no target then).
    let vk = prefix.join("vulkan");
    if vk.is_dir() && !Path::new("/usr/share/vulkan/icd.d").is_dir() {
        let mut icds: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&vk) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "json").unwrap_or(false) {
                    icds.push(p.to_string_lossy().into_owned());
                }
            }
        }
        icds.sort();
        if !icds.is_empty() {
            setenv_default("VK_ICD_FILENAMES", &icds.join(":"));
        }
    }
    // WebKitGTK applications: bubblewrap will not start in a container/userns.
    setenv_default("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
    // WebKit + NVIDIA in a container = broken GPU compositing (background only).
    // By default paint with Skia on the CPU and pass buffers via shm — can be
    // overridden by setting these variables yourself (e.g. to 0) to test the GPU path.
    setenv_default("WEBKIT_SKIA_ENABLE_CPU_RENDERING", "1");
    setenv_default("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1");
    if verbose {
        eprintln!("glibc-run: auto-env from runtime {}", prefix.display());
    }
}

fn find_loader(dir: &Path) -> Option<PathBuf> {
    for name in ["ld-linux-x86-64.so.2", "ld-linux.so.2"] {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Variables managed by auto-env — the list to clear in --host mode.
const MANAGED_ENV: &[&str] = &[
    "LD_LIBRARY_PATH",
    "GDK_PIXBUF_MODULE_FILE",
    "GDK_PIXBUF_MODULEDIR",
    "GSETTINGS_SCHEMA_DIR",
    "GIO_MODULE_DIR",
    "__EGL_VENDOR_LIBRARY_FILENAMES",
    "__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS",
    "__GLX_VENDOR_LIBRARY_NAME",
    "GBM_BACKEND",
    "GBM_BACKENDS_PATH",
    "VK_ICD_FILENAMES",
    "LIBGL_DRIVERS_PATH",
    "WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS",
    "WEBKIT_SKIA_ENABLE_CPU_RENDERING",
    "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
];

/// --host mode: clear the glibc-run environment (env vars + the glibc-run
/// entries in PATH) and run the command "the system way". The namespace mounts stay,
/// but thanks to the .musl-lib window and the ld-musl path file they are
/// neutral for musl.
fn host_mode(cmd: &[String]) -> ! {
    for k in MANAGED_ENV {
        std::env::remove_var(k);
    }
    if let Ok(path) = std::env::var("PATH") {
        let cleaned: Vec<&str> = path
            .split(':')
            .filter(|p| {
                !p.contains("/.glibc-run/")
                    && !p.ends_with("/glibc-run/tools")
                    && *p != NS_HELPER_BIN_DIR
            })
            .collect();
        std::env::set_var("PATH", cleaned.join(":"));
    }
    std::env::remove_var("GLIBC_RUN_NS");
    let prog = std::ffi::CString::new(cmd[0].as_bytes()).unwrap();
    let cargs: Vec<std::ffi::CString> = cmd
        .iter()
        .map(|s| std::ffi::CString::new(s.as_bytes()).unwrap())
        .collect();
    let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe {
        libc::execvp(prog.as_ptr(), ptrs.as_ptr());
    }
    eprintln!(
        "glibc-run --host: failed to run {}: {}",
        cmd[0],
        std::io::Error::last_os_error()
    );
    exit(127);
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(|s| s.as_str()) == Some("--host") {
        if raw.len() < 3 {
            eprintln!("glibc-run --host <command> [arguments...]");
            exit(2);
        }
        host_mode(&raw[2..]);
    }
    let mut i = 1;

    let mut prefix: Option<PathBuf> = std::env::var_os("GLIBC_RUN_PREFIX").map(PathBuf::from);
    let mut lib_dirs: Vec<PathBuf> = Vec::new();
    let mut user_lib_dirs: Vec<PathBuf> = Vec::new();
    let mut rootfs: Option<PathBuf> = None;
    let mut loader: Option<PathBuf> = None;
    let mut no_virt = false;
    let mut native = true;
    let mut no_env = false;
    let mut print_only = false;
    let mut verbose = false;

    while i < raw.len() {
        match raw[i].as_str() {
            "-h" | "--help" => usage(),
            "--prefix" => {
                i += 1;
                prefix = Some(PathBuf::from(raw.get(i).unwrap_or_else(|| usage())));
            }
            "--lib" => {
                i += 1;
                let d = PathBuf::from(raw.get(i).unwrap_or_else(|| usage()));
                user_lib_dirs.push(d.clone());
                lib_dirs.push(d);
            }
            "--rootfs" => {
                i += 1;
                rootfs = Some(PathBuf::from(raw.get(i).unwrap_or_else(|| usage())));
            }
            "--loader" => {
                i += 1;
                loader = Some(PathBuf::from(raw.get(i).unwrap_or_else(|| usage())));
            }
            "--no-virt" => no_virt = true,
            "--native" => native = true, // the default; flag kept for compatibility
            "--no-native" => native = false,
            "--no-env" => no_env = true,
            "--print" => print_only = true,
            "-v" | "--verbose" => verbose = true,
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with('-') => {
                eprintln!("glibc-run: unknown option: {s}");
                usage();
            }
            _ => break,
        }
        i += 1;
    }

    if i >= raw.len() {
        eprintln!("glibc-run: no binary to run");
        usage();
    }

    let binary = PathBuf::from(&raw[i]);
    let args: Vec<String> = raw[i + 1..].to_vec();

    // Prefix resolution: <prefix>/lib and <prefix>/rootfs as defaults.
    if let Some(pfx) = &prefix {
        let libdir = pfx.join("lib");
        if libdir.is_dir() {
            lib_dirs.insert(0, libdir);
        }
        if rootfs.is_none() {
            let rf = pfx.join("rootfs");
            if rf.is_dir() {
                rootfs = Some(rf);
            }
        }
    }

    // Default runtime: ~/.glibc-run/runtime (the tool's own runtime — user
    // data and HOME stay untouched). When $HOME is substituted (e.g. by a
    // sandboxed application launcher), we reach for the user's real home
    // from passwd — there is one global runtime.
    if prefix.is_none() {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home));
        }
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if !pw.is_null() && !(*pw).pw_dir.is_null() {
                let real = std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy();
                candidates.push(PathBuf::from(real.into_owned()));
            }
        }
        for home in candidates {
            let rt = home.join(".glibc-run/runtime");
            if rt.join("lib").is_dir() {
                prefix = Some(rt);
                break;
            }
        }
    }
    if let Some(pfx) = &prefix {
        // (complement for the default branch — --prefix was handled above)
        let libdir = pfx.join("lib");
        if libdir.is_dir() && !lib_dirs.contains(&libdir) {
            lib_dirs.insert(0, libdir);
        }
        if rootfs.is_none() {
            let rf = pfx.join("rootfs");
            if rf.is_dir() {
                rootfs = Some(rf);
            }
        }
        if !no_env {
            auto_env(pfx, &pfx.join("lib"), native, verbose);
        }
    }

    // The binary's directory goes onto the library path (for adjacent .so
    // files, e.g. libEOSSDK).
    if let Some(parent) = binary.parent() {
        if parent.as_os_str().is_empty() {
            lib_dirs.push(PathBuf::from("."));
        } else {
            lib_dirs.push(parent.to_path_buf());
        }
    }

    // Loader: explicit, or from the first lib directory, or from <prefix>/lib.
    let loader = loader
        .or_else(|| lib_dirs.iter().find_map(|d| find_loader(d)))
        .or_else(|| prefix.as_ref().and_then(|p| find_loader(&p.join("lib"))));

    let loader = match loader {
        Some(l) => l,
        None => {
            eprintln!(
                "glibc-run: ld-linux loader not found. Pass --loader or --prefix with a lib directory."
            );
            exit(1);
        }
    };

    if !binary.exists() {
        eprintln!("glibc-run: binary does not exist: {}", binary.display());
        exit(1);
    }

    // ELF diagnostics (informational only, never blocks).
    if verbose {
        match elf::read_elf(&binary) {
            Ok(info) => {
                eprintln!(
                    "glibc-run: {} (PIE={}), interp={:?}, NEEDED={}",
                    binary.display(),
                    info.is_pie,
                    info.interp,
                    info.needed.len()
                );
            }
            Err(e) => eprintln!("glibc-run: warning: could not parse ELF: {e}"),
        }
    }

    let argv0 = binary
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.to_string_lossy().into_owned());

    // Native mode: userns + loader substitution, no ld.so indirection.
    if native {
        let bundle = loader.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| {
            eprintln!("glibc-run: could not determine the glibc bundle directory (loader parent)");
            exit(1);
        });
        // Idempotency: we were invoked INSIDE an already mounted glibc-run
        // namespace with the SAME runtime (a glibc application launches
        // another wrapped program via PATH). The parent's mounts already
        // apply — loaders in /usr/lib, ld.so.cache in /etc, the
        // /usr/.musl-lib window — so any exec (glibc, musl, script) works
        // directly; the kernel dispatches via PT_INTERP. Nesting another
        // userns adds nothing and is sometimes limited. The GLIBC_RUN_NS
        // marker carries the runtime root; its credibility is confirmed by
        // /usr/.musl-lib, which exists only in our synthetic /usr (manually
        // exporting the marker outside the ns is not enough).
        let runtime_root = std::fs::canonicalize(&bundle)
            .ok()
            .and_then(|b| b.parent().map(Path::to_path_buf));
        let ns_root = std::env::var_os("GLIBC_RUN_NS")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .and_then(|p| std::fs::canonicalize(p).ok());
        let inside_same = runtime_root.is_some()
            && ns_root == runtime_root
            // a live bind of the musl window (a leftover empty directory does not qualify)
            && Path::new("/usr/.musl-lib/ld-musl-x86_64.so.1").is_file()
            // the glibc loader actually reachable at the ABI path (overlay active)
            && Path::new("/lib64/ld-linux-x86-64.so.2").exists();
        if print_only {
            if inside_same {
                println!("mode: native (already inside a glibc-run namespace — plain exec)");
            } else {
                println!("mode: native (userns)");
                println!("bind: {} -> /usr/lib", bundle.display());
            }
            println!("exec: {} {}", binary.display(), args.join(" "));
            if let Some(r) = &rootfs {
                println!("rootfs: {}", r.display());
            }
            exit(0);
        }
        if inside_same {
            // LD_LIBRARY_PATH exactly as in native::run — only explicit
            // directories from --lib, never anything from the runtime (the
            // lib32 lesson).
            if user_lib_dirs.is_empty() {
                std::env::remove_var("LD_LIBRARY_PATH");
            } else {
                let ld: Vec<String> = user_lib_dirs
                    .iter()
                    .map(|e| e.to_string_lossy().into_owned())
                    .collect();
                std::env::set_var("LD_LIBRARY_PATH", ld.join(":"));
            }
            if verbose {
                eprintln!(
                    "glibc-run: [native] already inside the namespace ({}) — exec without nesting",
                    runtime_root.as_deref().unwrap_or(Path::new("?")).display()
                );
            }
            use std::os::unix::ffi::OsStrExt;
            let prog = std::ffi::CString::new(binary.as_os_str().as_bytes()).unwrap();
            let mut cargv: Vec<std::ffi::CString> = Vec::with_capacity(args.len() + 1);
            cargv.push(std::ffi::CString::new(argv0.as_bytes()).unwrap());
            for a in &args {
                cargv.push(std::ffi::CString::new(a.as_bytes()).unwrap());
            }
            let mut ptrs: Vec<*const libc::c_char> = cargv.iter().map(|c| c.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            unsafe { libc::execv(prog.as_ptr(), ptrs.as_ptr()) };
            eprintln!(
                "glibc-run: exec inside the namespace failed: {}",
                std::io::Error::last_os_error()
            );
            exit(127);
        }
        let code = native::run(
            &bundle,
            rootfs.as_deref(),
            &binary,
            &argv0,
            &args,
            &user_lib_dirs,
            verbose,
        );
        exit(code);
    }

    let virt = !no_virt && rootfs.is_some();

    let spec = LaunchSpec {
        loader,
        lib_dirs,
        binary,
        argv0,
        args,
        virt,
        rootfs,
        verbose,
    };

    if print_only {
        println!("exec: {}", spec.loader_argv().join(" "));
        println!("virt: {}", spec.virt);
        if let Some(r) = &spec.rootfs {
            println!("rootfs: {}", r.display());
        }
        exit(0);
    }

    let code = runner::run(&spec);
    exit(code);
}
