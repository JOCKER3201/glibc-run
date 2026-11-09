# glibc-run

Uruchamia binarki **glibc** na systemie **musl** (np. Chimera Linux) — na zasadzie
wrappera, jak `wine` dla aplikacji Windows. Różnica wobec `wine`: niczego nie
emulujemy. Binarka glibc mówi do tego samego jądra Linuksa co musl — identyczne
syscalle. Różni się tylko warstwa userspace (glibc), która jest wolnym
oprogramowaniem (LGPL) i można ją po prostu **nosić ze sobą** w prefiksie.
Tam, gdzie `wine` musi reimplementować Windows, `glibc-run` przynosi prawdziwe glibc.

To czyni `gcompat` (protezę glibc na musl) zbędnym: zamiast tłumaczyć ABI,
używamy oryginalnego `ld-linux` i oryginalnych bibliotek.

> Autorstwo: narzędzie zaprojektowane i napisane przez AI (Claude, Anthropic)
> we współpracy z właścicielem repozytorium. Zob. nagłówek każdego pliku źródłowego.

## Jak to działa

Dwie warstwy:

1. **Rdzeń — uruchomienie przez loader.** Zamiast patchelfować binarkę
   (`--set-interpreter`), wołamy `ld-linux-x86-64.so.2` jako program:
   ```
   <prefix>/lib/ld-linux-x86-64.so.2 --library-path <prefix>/lib --argv0 <nazwa> <binarka> <args>
   ```
   Binarka pozostaje nietknięta i tylko do odczytu. Loader i biblioteki glibc
   leżą w prefiksie (`<prefix>/lib`).

2. **Wirtualizacja ścieżek — seccomp user-notify.** Filtr BPF przechwytuje
   `openat` i oddaje jądru deskryptor listenera. Proces-superwizor (rodzic) czyta
   ścieżkę z pamięci celu; jeśli jest absolutna i istnieje jej odpowiednik w
   nakładce `<prefix>/rootfs`, otwiera ten plik u siebie i **wstrzykuje deskryptor**
   do celu (ioctl `ADDFD` z flagą `SEND` — atomowo instaluje fd i ustawia go jako
   wynik syscalla). Reguła jest ogólna: *jeśli plik istnieje w `rootfs`, przesłania
   systemowy* — bez konfiguracji per-ścieżka. To rozwiązuje klasę problemów z
   zaszytymi ścieżkami configów i zasobów (`/etc/crypto-policies`, moduły GIO,
   cache gdk-pixbuf itd.).

Wszystko działa **bez roota, bez patchelf, bez kontenera** — listener seccomp
dostajemy nieuprzywilejowanie po ustawieniu `PR_SET_NO_NEW_PRIVS`.

## Użycie

```
glibc-run <binarka|skrypt> [argumenty aplikacji...]
```

Zero opcji. Narzędzie samo: znajduje runtime w `~/.glibc-run/runtime`
(także przy podmienionym $HOME — przez passwd), wybiera tryb natywny
(userns + overlay), ustawia środowisko według zawartości runtime'u
(GPU/glvnd/GBM/Vulkan, gdk-pixbuf, gsettings, gio, narzędzia GNU w PATH,
bash jako /bin/sh w przestrzeni, sandbox WebKita off, rendering WebKita
na CPU). Ustawia wyłącznie zmienne JESZCZE nieustawione — wszystko można
nadpisać. **HOME, XDG i dane użytkownika pozostają nietknięte** — aplikacje
widzą dokładnie ten sam home co zwykle.

Runtime buduje `setup-runtime.sh` (na hoście glibc). Opcje zaawansowane
(`--prefix`, `--lib`, `--loader`, `--no-native`, `--no-virt`, `--no-env`,
`--print`, `-v`): `glibc-run --help`.

Budowa (binarka wrappera jest musl-owa, by działała na systemie docelowym):

```
cargo build --release --target x86_64-unknown-linux-musl
```

## Stan i zasięg

Zweryfikowane na Chimera Linux (musl, bez gcompat):

| Test | Tryb | Wynik |
|---|---|---|
| Kontrola: binarka glibc wprost na musl | — | `not found` (jak należy) |
| Rdzeń: `jq` z pełnym closure (libjq/libonig/libm) | loader | działa, liczy filtry |
| Wirtualizacja: przekierowanie zaszytego `openat` na nakładkę | seccomp | działa |
| Claude Desktop (nietknięty .deb, Electron, 29 procesów) | `--native` | działa |

### Dwa tryby i dlaczego

Przy uruchomieniu przez loader `/proc/self/exe` wskazuje **loader**, nie binarkę
(potwierdzone empirycznie). Dla większości programów (np. `jq`) to bez znaczenia —
tryb domyślny (loader + seccomp-notify) wystarcza i jest najlżejszy: bez roota,
bez namespace'ów, bez patchelf.

Ale **Electron/Chromium** (Claude, Epic) używają `/proc/self/exe`, by (a) znaleźć
zasoby (`icudtl.dat`) i (b) re-exec-ować się dla procesów potomnych — oba
założenia łamią się przy uruchomieniu przez loader. `seccomp-notify` nie
przepisze też ścieżki w `execve` (helpery `/usr/libexec/webkit2gtk-4.1/…`).

Dlatego tryb **`--native`**: rootless `unshare(NEWUSER|NEWNS)` + bind-mount pakietu
glibc na `/usr/lib` (gdzie prowadzi `/lib64/ld-linux`). Binarka ładuje się wtedy
**natywnie**, `/proc/self/exe` = binarka, a self-reexec i ładowanie zasobów
działają. Zmiana widoczna tylko w tej przestrzeni nazw — system nietknięty.
Nakładka `rootfs` realizowana bind-mountami (helpery `execve` rozwiązane niejako
przy okazji).

Lekcja z integracji: moduły ładowane przez `dlopen` (NSS: `libsoftokn3`,
`libfreebl3`, `libnssckbi`; moduły GIO; loadery gdk-pixbuf) nie pojawiają się w
`ldd` — trzeba je dołożyć do pakietu ręcznie, inaczej bind przesłoni systemowe i
aplikacja padnie (NSS robi to twardym `FATAL`).

## Licencja

MIT (kod wrappera). Biblioteki glibc w prefiksie pozostają na LGPL i są tylko
dołączane, nie linkowane statycznie ani modyfikowane.
