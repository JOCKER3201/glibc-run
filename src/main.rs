//! glibc-run — uruchamia binarki glibc na systemie musl.
//!
//! Jak wine dla aplikacji Windows, ale zamiast reimplementować glibc, nosi ze
//! sobą prawdziwe glibc (prefix) i uruchamia binarkę przez jego loader. Opcjonalna
//! warstwa seccomp-notify przekierowuje zaszyte w kodzie ścieżki na nakładkę.

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
        "glibc-run — uruchamia binarki glibc na systemie musl (jak wine, ale nosi prawdziwe glibc)\n\
\n\
Użycie: glibc-run <binarka|skrypt> [argumenty...]\n\
\n\
Bez żadnych opcji: runtime z ~/.glibc-run/runtime, tryb natywny (userns+overlay),\n\
środowisko (GPU/glvnd, gdk-pixbuf, gsettings, gio, Vulkan, narzędzia GNU) ustawiane\n\
automatycznie według zawartości runtime'u — tylko zmienne JESZCZE nieustawione.\n\
HOME i dane użytkownika pozostają NIETKNIĘTE (jeden home jak zwykle).\n\
Wewnątrz już działającej przestrzeni glibc-run z tym samym runtime nie zagnieżdża\n\
userns — robi zwykły exec (znacznik: GLIBC_RUN_NS=<korzeń runtime'u>).\n\
\n\
Opcje (zaawansowane):\n\
  --prefix <dir>   Inny katalog runtime (domyślnie ~/.glibc-run/runtime\n\
                   lub $GLIBC_RUN_PREFIX); oczekuje <dir>/lib, opcjonalnie\n\
                   lib/lib32, tools, toolslibs, vulkan, rootfs\n\
  --lib <dir>      Dodatkowy katalog bibliotek (można wielokrotnie)\n\
  --rootfs <dir>   Nakładka ścieżek (domyślnie <prefix>/rootfs)\n\
  --loader <plik>  Jawna ścieżka ld-linux\n\
  --no-native      Tryb loadera zamiast natywnego (ld.so + --library-path;\n\
                   /proc/self/exe wskazuje wtedy loader — łamie Electron/CEF)\n\
  --no-virt        W trybie loadera: bez wirtualizacji ścieżek (seccomp)\n\
  --no-env         Nie ustawiaj automatycznego środowiska\n\
  --host <cmd...>  Uruchom polecenie PO SYSTEMOWEMU: czyści środowisko\n\
                   glibc-run (PATH/tools, GIO/GDK/GL/WebKit, LD_LIBRARY_PATH);\n\
                   do użycia z wnętrza przestrzeni (terminal w aplikacji)\n\
  --print          Wypisz plan uruchomienia i zakończ\n\
  -v, --verbose    Logi diagnostyczne na stderr\n\
  -h, --help       Ta pomoc\n"
    );
    exit(2);
}

/// Ustawia zmienną tylko, gdy użytkownik jej sam nie ustawił.
fn setenv_default(key: &str, val: &str) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, val);
    }
}

/// Automatyczne środowisko na podstawie zawartości runtime'u. Nigdy nie rusza
/// HOME ani XDG_* — dane użytkownika zostają te same co zwykle.
fn auto_env(prefix: &Path, libdir: &Path, verbose: bool) {
    // Narzędzia GNU (getopt/tar/...) przed BSD-owymi z musl — tylko dopisanie do PATH.
    let tools = prefix.join("tools");
    if tools.is_dir() {
        let old = std::env::var("PATH").unwrap_or_default();
        let t = tools.to_string_lossy();
        if !old.split(':').any(|p| p == t) {
            std::env::set_var("PATH", format!("{t}:{old}"));
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
    // Stos GPU NVIDIA — tylko gdy moduł jest w jądrze I wersja userspace
    // z runtime'u zgadza się z wersją modułu. Przy rozjeździe wymuszanie
    // vendora daje martwe EGL (ANGLE: "Failed to get system egl display");
    // lepszy jest czysty fallback na rendering programowy.
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
                "glibc-run: NVIDIA w jądrze {k}, w runtime {r} — wersje różne; \
                 rendering programowy (odśwież runtime pasującym userspace)."
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
        // Uniwersalny fallback: Mesa jest niezależna od wersji jądra (stabilne
        // ABI DRM) — AMD/Intel dostają akcelerację sprzętową, reszta llvmpipe.
        setenv_default("__GLX_VENDOR_LIBRARY_NAME", "mesa");
        set_if(
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            libdir.join("egl_vendor.d/50_mesa.json"),
        );
        set_if("LIBGL_DRIVERS_PATH", libdir.join("dri"));
    }
    // Vulkan: loader sam pomija ICD-y, których sterownik nie odpowiada —
    // wszystkie (nvidia + mesa radv/anv/lvp) trafiają w tryb natywny przez
    // bind na /usr/share/vulkan/icd.d (ścieżka dozwolona w sandboksie
    // Chromium). Zmienną ustawiamy tylko AWARYJNIE, gdy ten katalog nie
    // istnieje w systemie (wtedy bind nie ma celu).
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
    // Aplikacje WebKitGTK: bubblewrap nie wstanie w kontenerze/userns.
    setenv_default("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
    // WebKit + NVIDIA w kontenerze = zepsuta kompozycja GPU (samo tło).
    // Domyślnie maluj Skią na CPU i podawaj bufory przez shm — można nadpisać,
    // ustawiając te zmienne samodzielnie (np. na 0), by testować tor GPU.
    setenv_default("WEBKIT_SKIA_ENABLE_CPU_RENDERING", "1");
    setenv_default("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1");
    if verbose {
        eprintln!("glibc-run: auto-env z runtime {}", prefix.display());
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

/// Zmienne zarządzane przez auto-env — lista do czyszczenia w trybie --host.
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

/// Tryb --host: wyczyść środowisko glibc-run (env + wpis tools w PATH)
/// i uruchom polecenie "po systemowemu". Montowania przestrzeni zostają,
/// ale dzięki oknu .musl-lib i plikowi ld-musl path są dla musl neutralne.
fn host_mode(cmd: &[String]) -> ! {
    for k in MANAGED_ENV {
        std::env::remove_var(k);
    }
    if let Ok(path) = std::env::var("PATH") {
        let cleaned: Vec<&str> = path
            .split(':')
            .filter(|p| !p.contains("/.glibc-run/") && !p.ends_with("/glibc-run/tools"))
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
        "glibc-run --host: nie udało się uruchomić {}: {}",
        cmd[0],
        std::io::Error::last_os_error()
    );
    exit(127);
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(|s| s.as_str()) == Some("--host") {
        if raw.len() < 3 {
            eprintln!("glibc-run --host <polecenie> [argumenty...]");
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
            "--native" => native = true, // domyślne; flaga zostaje dla zgodności
            "--no-native" => native = false,
            "--no-env" => no_env = true,
            "--print" => print_only = true,
            "-v" | "--verbose" => verbose = true,
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with('-') => {
                eprintln!("glibc-run: nieznana opcja: {s}");
                usage();
            }
            _ => break,
        }
        i += 1;
    }

    if i >= raw.len() {
        eprintln!("glibc-run: brak binarki do uruchomienia");
        usage();
    }

    let binary = PathBuf::from(&raw[i]);
    let args: Vec<String> = raw[i + 1..].to_vec();

    // Rozwiązanie prefixu: <prefix>/lib i <prefix>/rootfs jako domyślne.
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

    // Domyślny runtime: ~/.glibc-run/runtime (sam runtime narzędzia — dane
    // użytkownika i HOME pozostają nietknięte). Gdy $HOME jest podmienione
    // (np. launcher aplikacji z piaskownicą), sięgamy po prawdziwy home
    // użytkownika z passwd — runtime jest jeden, globalny.
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
        // (uzupełnienie dla gałęzi domyślnej — --prefix obsłużono wyżej)
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
            auto_env(pfx, &pfx.join("lib"), verbose);
        }
    }

    // Katalog binarki do ścieżki bibliotek (dla sąsiednich .so, np. libEOSSDK).
    if let Some(parent) = binary.parent() {
        if parent.as_os_str().is_empty() {
            lib_dirs.push(PathBuf::from("."));
        } else {
            lib_dirs.push(parent.to_path_buf());
        }
    }

    // Loader: jawny, albo z pierwszego katalogu lib, albo z <prefix>/lib.
    let loader = loader
        .or_else(|| lib_dirs.iter().find_map(|d| find_loader(d)))
        .or_else(|| prefix.as_ref().and_then(|p| find_loader(&p.join("lib"))));

    let loader = match loader {
        Some(l) => l,
        None => {
            eprintln!(
                "glibc-run: nie znaleziono loadera ld-linux. Podaj --loader albo --prefix z katalogiem lib."
            );
            exit(1);
        }
    };

    if !binary.exists() {
        eprintln!("glibc-run: binarka nie istnieje: {}", binary.display());
        exit(1);
    }

    // Diagnostyka ELF (nie blokuje, tylko informuje).
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
            Err(e) => eprintln!("glibc-run: ostrzeżenie: nie sparsowano ELF: {e}"),
        }
    }

    let argv0 = binary
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.to_string_lossy().into_owned());

    // Tryb natywny: userns + podstawienie loadera, bez pośrednictwa ld.so.
    if native {
        let bundle = loader.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| {
            eprintln!("glibc-run: nie ustalono katalogu pakietu glibc (parent loadera)");
            exit(1);
        });
        // Idempotencja: wywołani WEWNĄTRZ już zmontowanej przestrzeni glibc-run
        // z TYM SAMYM runtime (glibc-owa aplikacja odpala przez PATH kolejny
        // owrapowany program). Mounty rodzica już obowiązują — loadery w
        // /usr/lib, ld.so.cache w /etc, okno /usr/.musl-lib — więc każdy exec
        // (glibc, musl, skrypt) działa wprost; dyspozycję robi jądro po
        // PT_INTERP. Zagnieżdżanie kolejnego userns nic nie wnosi, a bywa
        // limitowane. Znacznik GLIBC_RUN_NS niesie korzeń runtime'u;
        // wiarygodność potwierdza /usr/.musl-lib, istniejące wyłącznie w
        // naszej matrycy (ręczny export znacznika poza ns nie wystarczy).
        let runtime_root = std::fs::canonicalize(&bundle)
            .ok()
            .and_then(|b| b.parent().map(Path::to_path_buf));
        let ns_root = std::env::var_os("GLIBC_RUN_NS")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .and_then(|p| std::fs::canonicalize(p).ok());
        let inside_same = runtime_root.is_some()
            && ns_root == runtime_root
            // żywy bind okna musl (pusty katalog-niedobitek tego nie spełni)
            && Path::new("/usr/.musl-lib/ld-musl-x86_64.so.1").is_file()
            // loader glibc faktycznie osiągalny pod ścieżką ABI (overlay aktywny)
            && Path::new("/lib64/ld-linux-x86-64.so.2").exists();
        if print_only {
            if inside_same {
                println!("tryb: native (już w przestrzeni glibc-run — zwykły exec)");
            } else {
                println!("tryb: native (userns)");
                println!("bind: {} -> /usr/lib", bundle.display());
            }
            println!("exec: {} {}", binary.display(), args.join(" "));
            if let Some(r) = &rootfs {
                println!("rootfs: {}", r.display());
            }
            exit(0);
        }
        if inside_same {
            // LD_LIBRARY_PATH identycznie jak w native::run — wyłącznie
            // jawne katalogi z --lib, nigdy nic z runtime'u (lekcja lib32).
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
                    "glibc-run: [native] już wewnątrz przestrzeni ({}) — exec bez zagnieżdżania",
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
                "glibc-run: exec w przestrzeni nie powiódł się: {}",
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
