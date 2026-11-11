# glibc-run

Runs **glibc** binaries on a **musl** system (e.g. Chimera Linux) — as a
wrapper, like `wine` for Windows applications. The difference from `wine`:
nothing is emulated. A glibc binary talks to the same Linux kernel as musl —
identical syscalls. Only the userspace layer (glibc) differs, and it is free
software (LGPL) that can simply be **carried along** in a prefix.
Where `wine` has to reimplement Windows, `glibc-run` brings the real glibc.

This makes `gcompat` (the glibc shim for musl) unnecessary: instead of
translating the ABI, we use the original `ld-linux` and the original libraries.

> Authorship: tool designed and written by an AI (Claude, Anthropic)
> in collaboration with the repository owner. See the header of each source file.

## How it works

Two layers:

1. **Core — launching through the loader.** Instead of patchelf-ing the binary
   (`--set-interpreter`), we invoke `ld-linux-x86-64.so.2` as a program:
   ```
   <prefix>/lib/ld-linux-x86-64.so.2 --library-path <prefix>/lib --argv0 <name> <binary> <args>
   ```
   The binary stays untouched and read-only. The loader and the glibc
   libraries live in the prefix (`<prefix>/lib`).

2. **Path virtualization — seccomp user-notify.** A BPF filter intercepts
   `openat` and the kernel hands over a listener descriptor. The supervisor
   process (the parent) reads the path from the target's memory; if it is
   absolute and has a counterpart in the `<prefix>/rootfs` overlay, it opens
   that file itself and **injects the descriptor** into the target (the `ADDFD`
   ioctl with the `SEND` flag — atomically installs the fd and sets it as the
   syscall result). The rule is general: *if a file exists in `rootfs`, it
   shadows the system one* — no per-path configuration. This solves the class
   of problems with hardcoded config and resource paths
   (`/etc/crypto-policies`, GIO modules, the gdk-pixbuf cache, etc.).

Everything works **without root, without patchelf, without a container** — the
seccomp listener is obtained unprivileged after setting `PR_SET_NO_NEW_PRIVS`.

## Usage

```
glibc-run <binary|script> [application arguments...]
```

Zero options. The tool by itself: finds the runtime in `~/.glibc-run/runtime`
(even with a substituted $HOME — via passwd), picks native mode
(userns + overlay), sets up the environment from the runtime contents
(GPU/glvnd/GBM/Vulkan, gdk-pixbuf, gsettings, gio, GNU tools in PATH,
bash as /bin/sh inside the namespace, WebKit sandbox off, WebKit rendering
on the CPU). It only sets variables NOT yet set — everything can be
overridden. **HOME, XDG and user data stay untouched** — applications see
exactly the same home as usual.

The runtime is built by `setup-runtime.sh` (on a glibc host). Advanced options
(`--prefix`, `--lib`, `--loader`, `--no-native`, `--no-virt`, `--no-env`,
`--print`, `-v`): `glibc-run --help`.

Build (the wrapper binary is musl, so that it runs on the target system):

```
cargo build --release --target x86_64-unknown-linux-musl
```

## Status and scope

Verified on Chimera Linux (musl, no gcompat):

| Test | Mode | Result |
|---|---|---|
| Control: glibc binary directly on musl | — | `not found` (as expected) |
| Core: `jq` with its full closure (libjq/libonig/libm) | loader | works, evaluates filters |
| Virtualization: redirecting a hardcoded `openat` to the overlay | seccomp | works |
| Claude Desktop (untouched .deb, Electron, 29 processes) | `--native` | works |

### The two modes and why

When launched through the loader, `/proc/self/exe` points at the **loader**,
not the binary (confirmed empirically). For most programs (e.g. `jq`) this
does not matter — the default mode (loader + seccomp-notify) is enough and the
lightest: no root, no namespaces, no patchelf.

But **Electron/Chromium** (Claude, Epic) use `/proc/self/exe` to (a) find
resources (`icudtl.dat`) and (b) re-exec themselves for child processes — both
assumptions break when launching through the loader. `seccomp-notify` also
cannot rewrite the path in `execve` (helpers like
`/usr/libexec/webkit2gtk-4.1/…`).

Hence the **`--native`** mode: rootless `unshare(NEWUSER|NEWNS)` + a bind-mount
of the glibc bundle onto `/usr/lib` (where `/lib64/ld-linux` leads). The binary
then loads **natively**, `/proc/self/exe` = the binary, and self-reexec and
resource loading work. The change is visible only inside that namespace — the
system stays untouched. The `rootfs` overlay is realized with bind mounts
(the `execve` helpers get solved along the way).

Integration lesson: modules loaded via `dlopen` (NSS: `libsoftokn3`,
`libfreebl3`, `libnssckbi`; GIO modules; gdk-pixbuf loaders) do not show up in
`ldd` — they have to be added to the bundle manually, otherwise the bind
shadows the system ones and the application dies (NSS does it with a hard
`FATAL`).

## License

MIT (wrapper code). The glibc libraries in the prefix remain under the LGPL
and are only bundled, not statically linked nor modified.
