#!/bin/sh
# setup-runtime.sh — buduje runtime glibc-run z runtime'ów FLATPAKA.
#
# Flatpak jest jedynym uniwersalnym źródłem glibc na systemie musl, więc stąd
# bierzemy CAŁY glibc-owy userspace: rdzeń + GTK/WebKit (GNOME Platform), GPU
# (GL.default = Mesa, GL.nvidia-* = dopasowany do jądra), 32-bit (Compat.i386),
# Vulkan (MangoHud). ZERO zahardkodowanych ścieżek — lokalizacje przez
# `flatpak info -l`, wersje wykrywane, katalog wyjściowy parametryzowany.
#
# Użycie:  setup-runtime.sh [KATALOG_RUNTIME]
#          (domyślnie $GLIBC_RUN_RUNTIME lub ~/.glibc-run/runtime)
#
# Autorstwo: AI (Claude, Anthropic) + właściciel repozytorium.
set -eu

RT="${1:-${GLIBC_RUN_RUNTIME:-$HOME/.glibc-run/runtime}}"
FP="${FLATPAK:-flatpak}"
REMOTE="${FLATPAK_REMOTE:-flathub}"
ARCH=x86_64

command -v "$FP" >/dev/null || { echo "Potrzebny flatpak (FLATPAK=... by nadpisać)."; exit 1; }
command -v patchelf >/dev/null || { echo "Potrzebny patchelf (do interpretera helperów WebKita)."; exit 1; }

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }

# Zwraca ścieżkę .../files danego refa; pusto, gdy niezainstalowany.
files_of() { loc=$("$FP" info -l "$1" 2>/dev/null) && [ -n "$loc" ] && echo "$loc/files"; }

# Najnowszy zainstalowany ref pasujący do prefiksu (np. org.gnome.Platform/x86_64).
newest_ref() { "$FP" list --runtime --columns=ref 2>/dev/null | grep "^$1" | sort -V | tail -1; }

# Upewnia się, że runtime jest zainstalowany; przy braku instaluje z remote.
ensure() { # ensure <ref-or-prefix> [pełny-ref-do-instalacji]
    r=$(newest_ref "$1")
    if [ -z "$r" ]; then
        tgt="${2:-$1}"
        log "Instaluję $tgt z $REMOTE…"
        "$FP" install -y --noninteractive "$REMOTE" "$tgt" >/dev/null 2>&1 || \
            "$FP" install -y "$REMOTE" "$tgt"
        r=$(newest_ref "$1")
    fi
    echo "$r"
}

mkdir -p "$RT"/lib/lib32 "$RT"/loaders "$RT"/tools "$RT"/toolslibs \
         "$RT"/vulkan "$RT"/etc "$RT"/libexec "$RT"/rootfs/usr/bin \
         "$RT"/lib/egl_vendor.d "$RT"/lib/egl_external "$RT"/lib/gbm "$RT"/lib/dri

# --- 1. Rdzeń 64-bit: GNOME Platform (glibc + GTK + WebKit + NSS + GIO + pixbuf)
GNOME_REF=$(ensure "org.gnome.Platform/$ARCH" "org.gnome.Platform")
GN=$(files_of "$GNOME_REF"); [ -n "$GN" ] || { echo "Brak GNOME Platform"; exit 1; }
GNLIB="$GN/lib/$ARCH-linux-gnu"
log "Rdzeń z $GNOME_REF"
# biblioteki (multiarch -> płaski runtime/lib), bez loaderów
for f in "$GNLIB"/*.so*; do b=$(basename "$f"); case $b in ld-linux*) ;; *) cp -aL "$f" "$RT/lib/$b" 2>/dev/null || true;; esac; done
# podkatalogi modułowe
for d in gdk-pixbuf-2.0 gio gtk-3.0 webkit2gtk-4.1; do [ -d "$GNLIB/$d" ] && cp -aL "$GNLIB/$d" "$RT/lib/" 2>/dev/null || true; done
# schematy gsettings + glib
mkdir -p "$RT/lib/glib-2.0/schemas"
cp -aL "$GN"/share/glib-2.0/schemas/gschemas.compiled "$RT/lib/glib-2.0/schemas/" 2>/dev/null || true
# cache loaderów gdk-pixbuf z przepisaną ścieżką
PBC=$(find "$GN"/lib -name loaders.cache -path '*gdk-pixbuf*' 2>/dev/null | head -1)
if [ -n "$PBC" ]; then
    mkdir -p "$RT/lib/gdk-pixbuf"
    sed "s|[^\"]*/gdk-pixbuf-2.0/[0-9.]*/loaders|$RT/lib/gdk-pixbuf-2.0/$(basename "$(dirname "$(dirname "$PBC")")")/loaders|g" "$PBC" > "$RT/lib/gdk-pixbuf/loaders.cache" 2>/dev/null || cp "$PBC" "$RT/lib/gdk-pixbuf/loaders.cache"
fi
# helpery WebKita -> runtime/libexec; interpreter na standardowy (glibc-run go podstawia)
if [ -d "$GN/libexec/webkit2gtk-4.1" ]; then
    cp -aL "$GN/libexec/webkit2gtk-4.1" "$RT/libexec/"
    for h in "$RT"/libexec/webkit2gtk-4.1/WebKit*Process; do
        [ -f "$h" ] && patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 --remove-rpath "$h" 2>/dev/null || true
    done
    mkdir -p "$RT/lib/webkit2gtk-4.1/injected-bundle"
    cp -aL "$GN"/lib/*/webkit2gtk-4.1/injected-bundle/*.so "$RT/lib/webkit2gtk-4.1/injected-bundle/" 2>/dev/null || true
fi
# loader 64-bit -> loaders/ (overlay) ORAZ lib/ (żeby find_loader go znalazł)
cp -L "$GNLIB/ld-linux-x86-64.so.2" "$RT/loaders/"
cp -L "$GNLIB/ld-linux-x86-64.so.2" "$RT/lib/"
# narzędzia GNU + bash-jako-sh
for t in getopt realpath; do [ -f "$GN/bin/$t" ] && cp -L "$GN/bin/$t" "$RT/tools/"; done
[ -f "$GN/bin/tar" ] && { cp -L "$GN/bin/tar" "$RT/tools/tar.real"; printf '#!/bin/sh\nexport LD_LIBRARY_PATH=%s/toolslibs${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\nexec %s/tools/tar.real "$@"\n' "$RT" "$RT" > "$RT/tools/tar"; chmod +x "$RT/tools/tar"; }
printf '#!/bin/sh\nexit 0\n' > "$RT/tools/zenity"; chmod +x "$RT/tools/zenity"
cp -L "$GN/bin/bash" "$RT/rootfs/usr/bin/sh"
# plik ścieżek musl (okno .musl-lib) — informacyjnie; główny rozdział robi ld.so.cache
printf '/usr/.musl-lib\n/lib\n/usr/local/lib\n/usr/lib\n' > "$RT/etc/ld-musl-x86_64.path"

# --- 2. GPU 64-bit: Mesa (wszystkie sterowniki) + ewentualnie NVIDIA dopasowana
GLD=$(files_of "$(ensure "org.freedesktop.Platform.GL.default/$ARCH" "org.freedesktop.Platform.GL.default")")
if [ -n "$GLD" ]; then
    log "Mesa (uniwersalna) z GL.default"
    cp -anL "$GLD"/lib/*.so* "$RT/lib/" 2>/dev/null || true
    [ -d "$GLD/lib/dri" ] && cp -aL "$GLD"/lib/dri/* "$RT/lib/dri/" 2>/dev/null || true
    cp -f "$GLD"/glvnd/egl_vendor.d/*mesa*.json "$RT/lib/egl_vendor.d/" 2>/dev/null || true
    for j in "$GLD"/vulkan/icd.d/*.json; do [ -f "$j" ] && sed "s|/usr/lib/[^\"]*/GL/default/lib/|$RT/lib/|g" "$j" > "$RT/vulkan/$(basename "$j")"; done
fi
# NVIDIA: tylko gdy moduł jądra obecny — dopasuj wersję GL.nvidia-X-Y-Z do niego
if [ -r /proc/driver/nvidia/version ]; then
    NVVER=$(awk '{for(i=1;i<=NF;i++) if($i ~ /^[0-9]+\.[0-9]+\.[0-9]+$/){print $i; exit}}' /proc/driver/nvidia/version)
    NVREF_PREFIX="org.freedesktop.Platform.GL.nvidia-$(echo "$NVVER" | tr '.' '-')"
    NVD=$(files_of "$(ensure "$NVREF_PREFIX/$ARCH" "$NVREF_PREFIX")" 2>/dev/null || true)
    if [ -n "${NVD:-}" ]; then
        log "NVIDIA $NVVER (dopasowana do jądra)"
        cp -anL "$NVD"/lib/*.so* "$RT/lib/" 2>/dev/null || true
        [ -d "$NVD/lib/gbm" ] && cp -aL "$NVD"/lib/gbm/* "$RT/lib/gbm/" 2>/dev/null || true
        cp -f "$NVD"/glvnd/egl_vendor.d/*nvidia*.json "$RT/lib/egl_vendor.d/" 2>/dev/null || true
        cp -f "$NVD"/egl/egl_external_platform.d/*.json "$RT/lib/egl_external/" 2>/dev/null || true
        cp -f "$NVD"/vulkan/icd.d/*nvidia*.json "$RT/vulkan/" 2>/dev/null || true
    else
        echo "   (GL.nvidia-$(echo "$NVVER" | tr '.' '-') niedostępny — zostaje Mesa/software)"
    fi
fi

# --- 3. 32-bit (Steam): Compat.i386 + GL32, best-effort
C32=$(files_of "$(ensure "org.freedesktop.Platform.Compat.i386/$ARCH" "org.freedesktop.Platform.Compat.i386" 2>/dev/null || true)" 2>/dev/null || true)
if [ -n "${C32:-}" ]; then
    log "32-bit glibc (Compat.i386)"
    C32LIB="$C32/lib/i386-linux-gnu"
    for f in "$C32LIB"/*.so*; do b=$(basename "$f"); case $b in ld-linux*) cp -L "$f" "$RT/loaders/" ;; *) cp -aL "$f" "$RT/lib/lib32/$b" 2>/dev/null || true;; esac; done
    GL32=$(files_of "$(newest_ref "org.freedesktop.Platform.GL32.default/$ARCH")" 2>/dev/null || true)
    [ -n "${GL32:-}" ] && cp -anL "$GL32"/lib/*.so* "$RT/lib/lib32/" 2>/dev/null || true
    if [ -r /proc/driver/nvidia/version ] && [ -n "${NVVER:-}" ]; then
        NV32=$(files_of "$(newest_ref "org.freedesktop.Platform.GL32.nvidia-$(echo "$NVVER" | tr '.' '-')/$ARCH")" 2>/dev/null || true)
        [ -n "${NV32:-}" ] && cp -anL "$NV32"/lib/*.so* "$RT/lib/lib32/" 2>/dev/null || true
    fi
else
    echo "   (Compat.i386 niedostępny — Steam 32-bit nie zadziała; apki 64-bit OK)"
fi

# --- 4. Vulkan MangoHud (opcjonalnie, do nakładki FPS)
MH=$(files_of "$(newest_ref "org.freedesktop.Platform.VulkanLayer.MangoHud/$ARCH")" 2>/dev/null || true)
if [ -n "${MH:-}" ]; then
    mkdir -p "$RT/lib/mangohud"
    cp -aL "$MH"/lib/$ARCH-linux-gnu/libMangoHud*.so "$RT/lib/mangohud/" 2>/dev/null || true
fi

# --- 5. ld.so.cache (glibc!) — rdzeń rozdziału światów: overlay niesie TYLKO
# loadery, a biblioteki glibc rozwiązuje cache; musl-owy loader go nie czyta.
LDC="$GN/sbin/ldconfig"; [ -x "$LDC" ] || LDC="$GN/usr/sbin/ldconfig"; [ -x "$LDC" ] || LDC=ldconfig
if "$LDC" -C "$RT/etc/ld.so.cache" -X "$RT/lib" "$RT/lib/lib32" 2>/dev/null; then
    log "ld.so.cache: $(strings "$RT/etc/ld.so.cache" 2>/dev/null | grep -c "$RT/lib") wpisów"
else
    echo "   OSTRZEŻENIE: nie wygenerowano ld.so.cache (sprawdź ldconfig z flatpaka)"
fi

# rpath $ORIGIN na bibliotekach (żeby znajdowały się wzajemnie niezależnie od cache)
for so in "$RT"/lib/*.so* "$RT"/lib/lib32/*.so*; do
    [ -f "$so" ] && patchelf --force-rpath --set-rpath '$ORIGIN' "$so" 2>/dev/null || true
done

echo
log "Runtime gotowy: $RT"
printf '  lib: %s poz. (%s)   lib32: %s   loaders: %s   dri: %s\n' \
  "$(ls "$RT/lib" | wc -l)" "$(du -sh "$RT/lib" | cut -f1)" \
  "$(ls "$RT/lib/lib32" 2>/dev/null | wc -l)" "$(ls "$RT/loaders" | wc -l)" "$(ls "$RT/lib/dri" 2>/dev/null | wc -l)"
