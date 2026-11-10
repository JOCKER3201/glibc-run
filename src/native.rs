//! Tryb --native: uruchamia binarkę glibc NATYWNIE (bez pośrednictwa loadera),
//! dzięki czemu `/proc/self/exe` wskazuje binarkę, a nie ld.so.
//!
//! Mechanizm (rootless, bez roota na prawdziwym systemie):
//!  1. unshare(NEWUSER|NEWNS) — własna przestrzeń użytkownika i montowania.
//!  2. mapowanie uid/gid na root-w-ns (żeby móc montować).
//!  3. bind-mount pakietu glibc na /usr/lib. Ponieważ /lib64 → /usr/lib, zaszyty
//!     interpreter binarki (/lib64/ld-linux-x86-64.so.2) trafia na nasz loader, a
//!     biblioteki glibc przesłaniają musl-owe. Zmiana widoczna tylko w tej ns.
//!  4. execve binarki — jądro ładuje ją przez nasz interpreter.
//!
//! To rozwiązuje aplikacje zależne od /proc/self/exe (Electron/Chromium: Claude,
//! Epic), których nie da się uruchomić przez jawne wywołanie ld.so.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

/// Read-only overlay: `lower` przesłania swoje nazwy nad `target`, reszta
/// `target` pozostaje widoczna. Bez upperdir (nic nie zapisujemy).
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

/// Tmpfs-matryca katalogu systemowego: na świeżym tmpfs w `merged` odtwarza
/// każdy wpis `real` — katalogi przez rekursywny bind, symlinki przez
/// odtworzenie, zwykłe pliki przez bind pliku (po utworzeniu pustego celu).
/// Działa też pod rodzicem z zablokowanymi submountami, gdzie overlay z
/// `real` jako lowerem i bind-norec dają EINVAL.
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
        // gniazda/fifo w /etc pomijamy — nieistotne dla aplikacji
    }
    Ok(())
}

/// Bind BEZ rekursji: kopia katalogu bez jego submountów — przydatne jako
/// czysty lowerdir dla overlayfs (submounty w lowerze dają EINVAL).
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

/// Uruchamia binarkę natywnie w nowej przestrzeni nazw. Nie wraca przy sukcesie.
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
            "glibc-run: unshare(NEWUSER|NEWNS) nie powiodło się: {}\n\
             (nieuprzywilejowane userns muszą być dozwolone; w zagnieżdżonym userns, \
             np. wewnątrz distrobox, bywa to zablokowane — na gołym systemie działa)",
            std::io::Error::last_os_error()
        );
        return 1;
    }

    // Mapuj uid->ten sam uid: właściciel ns ma CAP_SYS_ADMIN w ns (montowanie
    // działa), a `id -u` zostaje bez zmian — np. steam.sh odmawia pracy jako root.
    let _ = fs::write("/proc/self/setgroups", "deny");
    if fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).is_err()
        || fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).is_err()
    {
        eprintln!(
            "glibc-run: zapis uid/gid_map nie powiódł się: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }

    // / prywatne — żeby bind nie wyciekł do macierzystej ns (best-effort).
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

    // Syntetyczny /usr (tmpfs-matryca: bind każdego wpisu, symlinki odtwarzane)
    // — deterministyczny także pod rodzicem z zablokowanymi submountami.
    // Dokłada: libexec z runtime'u (zaszyte ścieżki helperów, np. WebKit) oraz
    // ".musl-lib" = okno na PRAWDZIWY /usr/lib sprzed overlaya — dzięki temu
    // musl-owe dzieci (python/ctypes→libffi, narzędzia→libcrypto) dostają swoje
    // biblioteki przez /etc/ld-musl-x86_64.path, mimo że /usr/lib przykrywamy.
    let libexec_src = bundle
        .parent()
        .map(|p| p.join("libexec"))
        .filter(|p| p.is_dir());
    let musl_window = (|| -> Result<(), String> {
        let merged = bundle.parent().unwrap().join(format!(".usr-merged-{}", std::process::id()));
        let _ = fs::create_dir_all(&merged);
        synth_matrix(Path::new("/usr"), &merged)?;
        // okno musl: drugi bind prawdziwego /usr/lib (idempotentnie — przy
        // zagnieżdżeniu matryca skopiowała już wpis .musl-lib rodzica)
        let ml = merged.join(".musl-lib");
        if let Err(e) = fs::create_dir(&ml) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.to_string());
            }
        }
        mount_bind(Path::new("/usr/lib"), &ml.to_string_lossy())
            .map_err(|e| format!("bind .musl-lib: {e}"))?;
        // libexec z runtime'u
        if let Some(lx) = &libexec_src {
            let dst = merged.join("libexec");
            if dst.exists() {
                // realny libexec już zbindowany — dołóż nasz overlayem;
                // w zagnieżdżeniu overlay na odziedziczonym bindzie potrafi
                // dać ELOOP — wtedy bind na wierzch (nasze pliki przykrywają)
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
                eprintln!("glibc-run: [native] syntetyczny /usr (libexec + .musl-lib)");
            }
            true
        }
        Err(e) => {
            eprintln!("glibc-run: [native] syntetyczny /usr nieudany: {e}");
            false
        }
    };

    // Syntetyczny /etc: jak wyżej + plik /etc/ld-musl-x86_64.path kierujący
    // musl-owy loader najpierw do okna .musl-lib. Glibc tego pliku nie czyta.
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
            // Pliki z <runtime>/etc/ przesłaniają/uzupełniają systemowy /etc
            // w przestrzeni (np. ld.so.cache dla pressure-vessel Steama).
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
                    eprintln!("glibc-run: [native] syntetyczny /etc (ld-musl path)");
                }
            }
            Err(e) => eprintln!("glibc-run: [native] syntetyczny /etc nieudany: {e}"),
        }
    }

    // Loadery glibc -> /usr/lib. Overlay zawiera WYŁĄCZNIE ld-linux*
    // (interpreter musi istnieć ścieżkowo: /lib64/ld-linux-x86-64.so.2 →
    // /usr/lib). Wszystkie pozostałe biblioteki glibc rozwiązuje
    // /etc/ld.so.cache (wstrzykiwany do syntetycznego /etc) — mechanizm,
    // którego musl-owy loader NIE czyta. Dzięki temu musl-owe procesy widzą
    // nienaruszony /usr/lib (ich libunistring/libffi/libcrypto), a glibc-owe
    // swoje wersje przez cache. Zero kolizji sonames w obu światach.
    // (Stary wariant — pełny runtime/lib jako lower — zostaje jako fallback,
    //  gdy runtime nie ma jeszcze katalogu loaders/.)
    let bundle = fs::canonicalize(bundle).unwrap_or_else(|_| bundle.to_path_buf());
    let loaders = bundle
        .parent()
        .map(|p| p.join("loaders"))
        .filter(|p| p.join("ld-linux-x86-64.so.2").exists());
    let overlay_lower: &Path = loaders.as_deref().unwrap_or(&bundle);
    if verbose && loaders.is_some() {
        eprintln!("glibc-run: [native] overlay = tylko loadery (reszta przez ld.so.cache)");
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
            eprintln!("glibc-run: [native] overlay {} ponad /usr/lib", ovl_lower.display());
        }
    } else {
        let oe = std::io::Error::last_os_error();
        if let Err(e) = mount_bind(&ovl_lower, "/usr/lib") {
            eprintln!(
                "glibc-run: overlay nie powiódł się ({oe}), bind {} -> /usr/lib też nie: {e}",
                ovl_lower.display()
            );
            return 1;
        }
        if verbose {
            eprintln!(
                "glibc-run: [native] overlay niedostępny ({oe}) — pełny bind {} -> /usr/lib",
                ovl_lower.display()
            );
        }
    }

    // Vulkan: ICD-y z runtime'u podstawiamy w STANDARDOWEJ ścieżce — sandbox
    // Chromium (Brave itp.) pozwala procesowi GPU czytać /usr/share/vulkan,
    // ale nie $HOME/.glibc-run (stąd "Failed to open JSON" mimo istnienia).
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
                Err(e) => eprintln!("glibc-run: [native] bind vulkan ICD nieudany: {e}"),
            }
        }
    }

    // Nakładka rootfs: bind każdego pliku/katalogu na istniejący cel systemowy.
    // (Dla celów, które nie istnieją — np. /usr/libexec/webkit — bind się nie uda;
    //  to wymaga overlay i jest poza zakresem MVP; logujemy pominięcie.)
    if let Some(rf) = rootfs {
        bind_rootfs(rf, rf, verbose);
    }

    // LD_LIBRARY_PATH: w trybie natywnym WSZYSTKIE biblioteki glibc (64- i
    // 32-bit) rozwiązuje /etc/ld.so.cache — loadery glibc czytają cache i same
    // wybierają wpis o właściwej klasie ELF. NIE wolno dokładać tu katalogów
    // glibc (ani lib, ani lib32): musl-owy loader TEŻ czyta LD_LIBRARY_PATH i —
    // wbrew częstemu założeniu — NIE pomija pliku o złej klasie ELF, tylko
    // zwraca ENOEXEC na pierwszym trafieniu po nazwie. lib32 z Compat.i386
    // niesie m.in. 32-bitowe libreadline/libtinfo o tych samych sonamkach co
    // host-owe musl-owe; przez LD_LIBRARY_PATH zabijały każde musl-owe dziecko
    // (np. bash: „libreadline.so.8: Exec format error"). Zostają wyłącznie
    // jawne katalogi użytkownika z --lib — to jego świadoma decyzja.
    let mut ld = Vec::new();
    for e in extra_ld {
        ld.push(e.to_string_lossy().into_owned());
    }
    if ld.is_empty() {
        std::env::remove_var("LD_LIBRARY_PATH");
    } else {
        std::env::set_var("LD_LIBRARY_PATH", ld.join(":"));
    }

    // Znacznik dla narzędzi/skryptów ORAZ idempotencji (main.rs): jesteśmy w
    // przestrzeni glibc-run; wartość = korzeń runtime'u. Kolejne wywołanie
    // glibc-run z tym samym runtime (glibc-owa aplikacja odpalająca przez
    // PATH inny owrapowany program) nie zagnieżdża wtedy userns, tylko robi
    // zwykły exec — mounty odziedziczone po nas już obowiązują.
    match bundle.parent() {
        Some(rt) => std::env::set_var("GLIBC_RUN_NS", rt),
        None => std::env::set_var("GLIBC_RUN_NS", "1"),
    }

    // Fork: dziecko exec-uje natywnie (jego /proc/self/exe = binarka),
    // rodzic zostaje i PRZEKAZUJE sygnały terminala (Ctrl+C itd.) dziecku.
    // Niektóre aplikacje (Electron) potrafią zignorować SIGINT — drugi
    // sygnał eskalujemy do SIGKILL, żeby ^C ^C zawsze działało.
    let child = unsafe { libc::fork() };
    if child < 0 {
        eprintln!(
            "glibc-run: fork nie powiódł się: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    if child == 0 {
        // --- dziecko: execve natywny ---
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
            "glibc-run: execv natywny nie powiódł się: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(127);
    }

    // --- rodzic: przekaźnik sygnałów + wait ---
    unsafe {
        CHILD_PID.store(child, std::sync::atomic::Ordering::SeqCst);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, forward_signal as usize as libc::sighandler_t);
        }
        // Ctrl+C z terminala i tak trafia do grupy procesów (więc też do
        // dziecka); handler łapie powtórkę i eskaluje.
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
            // powtórny sygnał: aplikacja nie reaguje — ubij całą grupę dziecka
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Rekurencyjnie bind-mountuje pliki z nakładki na ich systemowe ścieżki.
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
            // Cel istnieje: plik -> bind pliku; katalog -> wejdź głębiej (żeby nie
            // przesłaniać całego katalogu systemowego).
            if is_dir {
                bind_rootfs(root, &path, verbose);
            } else if let Err(e) = mount_bind(&path, &sys.to_string_lossy()) {
                if verbose {
                    eprintln!("glibc-run: [native] bind rootfs {} pominięty: {e}", sys.display());
                }
            } else if verbose {
                eprintln!("glibc-run: [native] rootfs {} -> {}", path.display(), sys.display());
            }
        } else if verbose {
            eprintln!(
                "glibc-run: [native] cel {} nie istnieje — pominięto (wymaga overlay)",
                sys.display()
            );
        }
    }
}
