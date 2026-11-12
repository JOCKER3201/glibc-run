#!/bin/sh
# setup-runtime.sh — builds the glibc-run runtime from FLATPAK runtimes.
#
# Flatpak is the only universal source of glibc on a musl system, so we take
# the WHOLE glibc userspace from it: core + GTK/WebKit (GNOME Platform), GPU
# (GL.default = Mesa, GL.nvidia-* = matched to the kernel), 32-bit
# (Compat.i386), Vulkan (MangoHud). ZERO hardcoded paths — locations via
# `flatpak info -l`, versions detected, output directory parameterized.
#
# Usage:  setup-runtime.sh [RUNTIME_DIR]
#         (default $GLIBC_RUN_RUNTIME or ~/.glibc-run/runtime)
#
# Authorship: AI (Claude, Anthropic) + the repository owner.
set -eu

RT="${1:-${GLIBC_RUN_RUNTIME:-$HOME/.glibc-run/runtime}}"
FP="${FLATPAK:-flatpak}"
REMOTE="${FLATPAK_REMOTE:-flathub}"
ARCH=x86_64

command -v "$FP" >/dev/null || { echo "flatpak is required (FLATPAK=... to override)."; exit 1; }
command -v patchelf >/dev/null || { echo "patchelf is required (for the WebKit helper interpreters)."; exit 1; }

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }

# Prints the .../files path of a given ref; empty when not installed.
files_of() { loc=$("$FP" info -l "$1" 2>/dev/null) && [ -n "$loc" ] && echo "$loc/files"; }

# Newest installed ref matching the prefix (e.g. org.gnome.Platform/x86_64).
newest_ref() { "$FP" list --runtime --columns=ref 2>/dev/null | grep "^$1" | sort -V | tail -1; }

# Makes sure the runtime is installed; installs from the remote when missing.
ensure() { # ensure <ref-or-prefix> [full-ref-to-install]
    r=$(newest_ref "$1")
    if [ -z "$r" ]; then
        tgt="${2:-$1}"
        log "Installing $tgt from $REMOTE…"
        "$FP" install -y --noninteractive "$REMOTE" "$tgt" >/dev/null 2>&1 || \
            "$FP" install -y "$REMOTE" "$tgt"
        r=$(newest_ref "$1")
    fi
    echo "$r"
}

mkdir -p "$RT"/lib/lib32 "$RT"/loaders "$RT"/tools "$RT"/toolslibs \
         "$RT"/vulkan "$RT"/etc "$RT"/libexec "$RT"/rootfs/usr/bin \
         "$RT"/lib/egl_vendor.d "$RT"/lib/egl_external "$RT"/lib/gbm "$RT"/lib/dri

# --- 1. 64-bit core: GNOME Platform (glibc + GTK + WebKit + NSS + GIO + pixbuf)
GNOME_REF=$(ensure "org.gnome.Platform/$ARCH" "org.gnome.Platform")
GN=$(files_of "$GNOME_REF"); [ -n "$GN" ] || { echo "GNOME Platform missing"; exit 1; }
GNLIB="$GN/lib/$ARCH-linux-gnu"
log "Core from $GNOME_REF"
# libraries (multiarch -> flat runtime/lib), loaders excluded
for f in "$GNLIB"/*.so*; do b=$(basename "$f"); case $b in ld-linux*) ;; *) cp -aL "$f" "$RT/lib/$b" 2>/dev/null || true;; esac; done
# module subdirectories
for d in gdk-pixbuf-2.0 gio gtk-3.0 webkit2gtk-4.1; do [ -d "$GNLIB/$d" ] && cp -aL "$GNLIB/$d" "$RT/lib/" 2>/dev/null || true; done
# gsettings + glib schemas
mkdir -p "$RT/lib/glib-2.0/schemas"
cp -aL "$GN"/share/glib-2.0/schemas/gschemas.compiled "$RT/lib/glib-2.0/schemas/" 2>/dev/null || true
# gdk-pixbuf loader cache with the path rewritten
PBC=$(find "$GN"/lib -name loaders.cache -path '*gdk-pixbuf*' 2>/dev/null | head -1)
if [ -n "$PBC" ]; then
    mkdir -p "$RT/lib/gdk-pixbuf"
    sed "s|[^\"]*/gdk-pixbuf-2.0/[0-9.]*/loaders|$RT/lib/gdk-pixbuf-2.0/$(basename "$(dirname "$(dirname "$PBC")")")/loaders|g" "$PBC" > "$RT/lib/gdk-pixbuf/loaders.cache" 2>/dev/null || cp "$PBC" "$RT/lib/gdk-pixbuf/loaders.cache"
fi
# WebKit helpers -> runtime/libexec; interpreter set to the standard one (glibc-run supplies it)
if [ -d "$GN/libexec/webkit2gtk-4.1" ]; then
    cp -aL "$GN/libexec/webkit2gtk-4.1" "$RT/libexec/"
    for h in "$RT"/libexec/webkit2gtk-4.1/WebKit*Process; do
        [ -f "$h" ] && patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 --remove-rpath "$h" 2>/dev/null || true
    done
    mkdir -p "$RT/lib/webkit2gtk-4.1/injected-bundle"
    cp -aL "$GN"/lib/*/webkit2gtk-4.1/injected-bundle/*.so "$RT/lib/webkit2gtk-4.1/injected-bundle/" 2>/dev/null || true
fi
# 64-bit loader -> loaders/ (overlay) AND lib/ (so find_loader can find it)
cp -L "$GNLIB/ld-linux-x86-64.so.2" "$RT/loaders/"
cp -L "$GNLIB/ld-linux-x86-64.so.2" "$RT/lib/"
# GNU tools + bash-as-sh
for t in getopt realpath; do [ -f "$GN/bin/$t" ] && cp -L "$GN/bin/$t" "$RT/tools/"; done
[ -f "$GN/bin/tar" ] && { cp -L "$GN/bin/tar" "$RT/tools/tar.real"; printf '#!/bin/sh\nexport LD_LIBRARY_PATH=%s/toolslibs${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\nexec %s/tools/tar.real "$@"\n' "$RT" "$RT" > "$RT/tools/tar"; chmod +x "$RT/tools/tar"; }
printf '#!/bin/sh\nexit 0\n' > "$RT/tools/zenity"; chmod +x "$RT/tools/zenity"
cp -L "$GN/bin/bash" "$RT/rootfs/usr/bin/sh"
# musl path file (the .musl-lib window) — informational; the real separation is done by ld.so.cache
printf '/usr/.musl-lib\n/lib\n/usr/local/lib\n/usr/lib\n' > "$RT/etc/ld-musl-x86_64.path"
# Desktop integration: files the flat `cp *.so*` from GNOME Platform loses, without
# which GTK/Chromium loses the button layout+theme (dconf) and sound (libpulse).
# libpulsecommon: required by libpulse.so.0, lives in the pulseaudio/ subdirectory.
for f in $(find "$GN/lib" -name 'libpulsecommon-*.so' 2>/dev/null); do cp -fL "$f" "$RT/lib/" 2>/dev/null || true; done
# dconf: the GSettings backend (theme + gtk-decoration-layout). GNOME Platform does NOT
# ship it — look for a glibc libdconfsettings.so in installed flatpaks / local bundles.
D=$(find "$HOME/.local/share/flatpak" "$HOME"/.git/*musl*/app -name libdconfsettings.so 2>/dev/null | head -1)
if [ -n "$D" ]; then
    cp -fL "$D" "$RT/lib/gio/modules/"
    L=$(find "$HOME/.local/share/flatpak" -name 'libdconf.so.1' 2>/dev/null | head -1); [ -n "$L" ] && cp -fL "$L"* "$RT/lib/"
    [ -x "$GN/bin/gio-querymodules" ] && "$GN/bin/gio-querymodules" "$RT/lib/gio/modules" 2>/dev/null || true
else
    echo "   WARNING: libdconfsettings.so missing — GTK/Chromium will lose the dconf theme + button layout"
fi
# ALSA -> PipeWire (audio fallback when Chromium cannot get in via pulse).
mkdir -p "$RT/lib/alsa-lib"
AP=$(find "$(files_of "$(newest_ref "org.freedesktop.Platform/$ARCH")" 2>/dev/null)" "$GN" -name libasound_module_pcm_pipewire.so 2>/dev/null | head -1)
[ -n "$AP" ] && cp -fL "$AP" "$RT/lib/alsa-lib/" 2>/dev/null || true

# --- 2. 64-bit GPU: Mesa (all drivers) + optionally a matching NVIDIA
GLD=$(files_of "$(ensure "org.freedesktop.Platform.GL.default/$ARCH" "org.freedesktop.Platform.GL.default")")
if [ -n "$GLD" ]; then
    log "Mesa (universal) from GL.default"
    cp -anL "$GLD"/lib/*.so* "$RT/lib/" 2>/dev/null || true
    [ -d "$GLD/lib/dri" ] && cp -aL "$GLD"/lib/dri/* "$RT/lib/dri/" 2>/dev/null || true
    cp -f "$GLD"/glvnd/egl_vendor.d/*mesa*.json "$RT/lib/egl_vendor.d/" 2>/dev/null || true
    for j in "$GLD"/vulkan/icd.d/*.json; do [ -f "$j" ] && sed "s|/usr/lib/[^\"]*/GL/default/lib/|$RT/lib/|g" "$j" > "$RT/vulkan/$(basename "$j")"; done
fi
# NVIDIA: only when the kernel module is present — match the GL.nvidia-X-Y-Z version to it
if [ -r /proc/driver/nvidia/version ]; then
    NVVER=$(awk '{for(i=1;i<=NF;i++) if($i ~ /^[0-9]+\.[0-9]+\.[0-9]+$/){print $i; exit}}' /proc/driver/nvidia/version)
    NVREF_PREFIX="org.freedesktop.Platform.GL.nvidia-$(echo "$NVVER" | tr '.' '-')"
    NVD=$(files_of "$(ensure "$NVREF_PREFIX/$ARCH" "$NVREF_PREFIX")" 2>/dev/null || true)
    if [ -n "${NVD:-}" ]; then
        log "NVIDIA $NVVER (matched to the kernel)"
        cp -anL "$NVD"/lib/*.so* "$RT/lib/" 2>/dev/null || true
        [ -d "$NVD/lib/gbm" ] && cp -aL "$NVD"/lib/gbm/* "$RT/lib/gbm/" 2>/dev/null || true
        cp -f "$NVD"/glvnd/egl_vendor.d/*nvidia*.json "$RT/lib/egl_vendor.d/" 2>/dev/null || true
        cp -f "$NVD"/egl/egl_external_platform.d/*.json "$RT/lib/egl_external/" 2>/dev/null || true
        cp -f "$NVD"/vulkan/icd.d/*nvidia*.json "$RT/vulkan/" 2>/dev/null || true
    else
        echo "   (GL.nvidia-$(echo "$NVVER" | tr '.' '-') unavailable — staying with Mesa/software)"
    fi
fi

# --- 3. 32-bit (Steam): Compat.i386 + GL32, best effort
C32=$(files_of "$(ensure "org.freedesktop.Platform.Compat.i386/$ARCH" "org.freedesktop.Platform.Compat.i386" 2>/dev/null || true)" 2>/dev/null || true)
if [ -n "${C32:-}" ]; then
    log "32-bit glibc (Compat.i386)"
    # Compat.i386 keeps the libraries AND the 32-bit loader in the files/ root.
    for f in "$C32"/*.so* "$C32"/ld-linux.so.2; do
        [ -f "$f" ] || continue; b=$(basename "$f")
        case $b in ld-linux*) cp -L "$f" "$RT/loaders/"; cp -L "$f" "$RT/lib/lib32/" ;;
                   *) cp -aL "$f" "$RT/lib/lib32/$b" 2>/dev/null || true;; esac
    done
    # GL32 (Mesa + 32-bit NVIDIA): libraries in lib/ or in the root
    nvtag=$(echo "${NVVER:-0}" | tr '.' '-')
    for src in "$(files_of "$(newest_ref "org.freedesktop.Platform.GL32.default/$ARCH")" 2>/dev/null)" \
               "$(files_of "$(newest_ref "org.freedesktop.Platform.GL32.nvidia-$nvtag/$ARCH")" 2>/dev/null)"; do
        [ -n "$src" ] || continue
        for sub in lib .; do for f in "$src/$sub"/*.so*; do [ -f "$f" ] && cp -anL "$f" "$RT/lib/lib32/$(basename "$f")" 2>/dev/null || true; done; done
    done
else
    echo "   (Compat.i386 unavailable — 32-bit Steam will not work; 64-bit apps OK)"
fi

# --- 4. MangoHud Vulkan layer (optional, for the FPS overlay)
MH=$(files_of "$(newest_ref "org.freedesktop.Platform.VulkanLayer.MangoHud/$ARCH")" 2>/dev/null || true)
if [ -n "${MH:-}" ]; then
    mkdir -p "$RT/lib/mangohud"
    cp -aL "$MH"/lib/$ARCH-linux-gnu/libMangoHud*.so "$RT/lib/mangohud/" 2>/dev/null || true
fi

# --- 5. ld.so.cache (glibc!) — the core of the world separation: the overlay
# carries ONLY the loaders, glibc libraries are resolved via the cache; the
# musl loader does not read it.
LDC="$GN/sbin/ldconfig"; [ -x "$LDC" ] || LDC="$GN/usr/sbin/ldconfig"; [ -x "$LDC" ] || LDC=ldconfig
if "$LDC" -C "$RT/etc/ld.so.cache" -X "$RT/lib" "$RT/lib/lib32" 2>/dev/null; then
    log "ld.so.cache: $(strings "$RT/etc/ld.so.cache" 2>/dev/null | grep -c "$RT/lib") entries"
else
    echo "   WARNING: ld.so.cache was not generated (check the flatpak ldconfig)"
fi

# glibc ldconfig for nested pressure-vessel containers (Steam Linux Runtime).
# pressure-vessel rebuilds the container's ld.so.cache with the provider's
# ldconfig, found through PATH and accepted only from a path under /usr; a
# musl host has just a no-op stub there, which leaves the container without a
# cache. glibc-run overlays libexec/ on /usr/libexec and puts
# /usr/libexec/glibc-run on PATH, so this copy is what the containers run. It
# has to be the static binary from the same glibc as the runtime, hence only
# the flatpak one qualifies.
if [ "$LDC" != ldconfig ]; then
    mkdir -p "$RT/libexec/glibc-run"
    cp -L "$LDC" "$RT/libexec/glibc-run/ldconfig"
fi

# --- 5b. 32-bit glibc in /usr/lib/i386-linux-gnu — the SYSTEM path of the
# i386 glibc loader, which musl does NOT search (it does not know the triplet;
# it looks in /lib, /usr/lib, the .musl-lib window). The overlay carries
# loaders/ onto /usr/lib, so loaders/i386-linux-gnu/ → /usr/lib/i386-linux-gnu/.
# This keeps 32-bit programs working even when the ld.so.cache has holes:
# pressure-vessel/Steam (capsule-capture-libs i386, reaper) otherwise die with
# "libc.so.6: cannot open" and exit 127 BEFORE Proton starts (a missing proton
# log is the symptom). Those holes came from patchelf'ed lib32 files that
# ldconfig could not parse (see the rpath note at the end of this script), not
# from cross-arch ldconfig as assumed earlier; the cache is complete now and
# this stays as a safety net. CAUTION: lib32 must NOT be put into
# LD_LIBRARY_PATH — musl reads it too and dies on 32-bit sonames (ENOEXEC, a
# bricked bash). The symlinks are absolute — like ld.so.cache they need
# regenerating after the runtime is moved/renamed.
if [ -d "$RT/lib/lib32" ]; then
    mkdir -p "$RT/loaders/i386-linux-gnu"
    for f in "$RT"/lib/lib32/*.so*; do
        [ -e "$f" ] && ln -sf "$f" "$RT/loaders/i386-linux-gnu/$(basename "$f")"
    done
    log "32-bit glibc → /usr/lib/i386-linux-gnu ($(ls "$RT/loaders/i386-linux-gnu" 2>/dev/null | wc -l) libs, invisible to musl)"
fi

# $ORIGIN rpath on the libraries (so they find each other independently of the cache)
# 64-bit only. When patchelf has to move .dynstr into a new segment, 32-bit
# libraries end up with a layout that glibc's ldconfig rejects ("file ... is
# truncated"), which silently drops them from every ld.so.cache built later:
# the runtime's own when regenerated, and the one pressure-vessel builds inside
# each container. A third of lib32 went missing that way, libc.so.6 included.
# lib32 is fully covered by the cache and /usr/lib/i386-linux-gnu, so it does
# not need the rpath.
for so in "$RT"/lib/*.so*; do
    [ -f "$so" ] && patchelf --force-rpath --set-rpath '$ORIGIN' "$so" 2>/dev/null || true
done
# Guard against the same failure on the 64-bit side.
if [ "$LDC" != ldconfig ]; then
    bad=$("$LDC" -v -N -X -f /dev/null "$RT/lib" "$RT/lib/lib32" 2>&1 | grep -c 'is truncated' || true)
    [ "$bad" -eq 0 ] || echo "   WARNING: $bad libraries are unreadable for ldconfig after patchelf (they will be missing from ld.so.cache)"
fi

echo
log "Runtime ready: $RT"
printf '  lib: %s entries (%s)   lib32: %s   loaders: %s   dri: %s\n' \
  "$(ls "$RT/lib" | wc -l)" "$(du -sh "$RT/lib" | cut -f1)" \
  "$(ls "$RT/lib/lib32" 2>/dev/null | wc -l)" "$(ls "$RT/loaders" | wc -l)" "$(ls "$RT/lib/dri" 2>/dev/null | wc -l)"
