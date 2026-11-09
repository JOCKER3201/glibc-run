#!/usr/bin/env bash
# Składa kanoniczny runtime glibc-run w ~/.glibc-run/runtime ze sprawdzonych
# komponentów zbudowanych podczas portów Claude/Epic/Steam na musl.
# Uruchamiać NA HOŚCIE (glibc). To runtime narzędzia — ŻADNYCH danych użytkownika.
set -euo pipefail

RT=${1:-$HOME/.glibc-run/runtime}
CL=/home/user/.git/claude-desktop-musl/app/glibc      # glibc 64 + GUI + NSS + X11 + 64-bit GL/NVIDIA
EP=/home/user/.git/epic-musl/app/glibc                # moduły GIO (TLS), webkit
GPU=/home/user/.git/claude-desktop-musl/gpu-nvidia    # pełny stos NVIDIA 64 + configi
ST=/home/user/.git/steam-musl/prefix                  # lib32, tools, toolslibs, vulkan, rootfs

mkdir -p "$RT"/lib/lib32 "$RT"/tools "$RT"/toolslibs "$RT"/vulkan "$RT"/rootfs/usr/bin

# 1. Baza 64-bit: bundle Claude'a (zawiera też gdk-pixbuf/, glib-2.0/, hooki NSS)
cp -a "$CL"/. "$RT"/lib/

# 2. Moduły GIO z bundla Epica (TLS przez glib-networking)
mkdir -p "$RT"/lib/gio
cp -a "$EP"/gio/modules "$RT"/lib/gio/

# 2b. Stos WebKitGTK 4.1 z bundla Epica (webkit, soup3, gstreamer…) —
#     tylko biblioteki, których jeszcze nie ma (bez nadpisywania bazy)
for so in "$EP"/*.so*; do
  b=$(basename "$so"); [ -e "$RT/lib/$b" ] || cp -a "$so" "$RT/lib/$b"
done

# 3. Stos GPU NVIDIA 64 + backendy GBM + configi glvnd/EGL
cp -an "$GPU"/*.so.* "$RT"/lib/ 2>/dev/null || true
for l in "$GPU"/*.so.*; do b=$(basename "$l"); [ -e "$RT/lib/$b" ] || cp -L "$l" "$RT/lib/$b"; done
# symlinki SONAME
( cd "$RT/lib" && for f in libGLX_nvidia libEGL_nvidia; do ln -sf $f.so.*.* $f.so.0 2>/dev/null || true; done )
cp -a "$GPU"/gbm "$RT"/lib/
mkdir -p "$RT"/lib/egl_vendor.d "$RT"/lib/egl_external
cp "$GPU"/egl_vendor.d/*.json "$RT"/lib/egl_vendor.d/
cp "$GPU"/egl_external/*.json "$RT"/lib/egl_external/

# 4. Rdzeń 32-bit + 32-bit GL/NVIDIA (ze Steama)
cp -a "$ST"/lib/lib32/. "$RT"/lib/lib32/
cp -L "$ST"/lib/ld-linux.so.2 "$RT"/lib/   # loader 32-bit

# 5. Narzędzia GNU + ich odseparowane biblioteki + Vulkan ICD + bash-jako-sh
cp -a "$ST"/tools/. "$RT"/tools/
cp -a "$ST"/toolslibs/. "$RT"/toolslibs/
cp "$ST"/vulkan/*.json "$RT"/vulkan/
cp -L /usr/bin/bash "$RT"/rootfs/usr/bin/sh
cp -L /usr/lib64/libtinfo.so.6 "$RT"/lib/ 2>/dev/null || true

# 6. Przepisz ścieżki w loaders.cache i wrapperze tara na runtime
PB="$RT/lib/gdk-pixbuf"
if [ -f "$PB/loaders.cache" ]; then
  sed -i "s|/home/user/.git/claude-desktop-musl/app/glibc/gdk-pixbuf|$PB|g" "$PB/loaders.cache"
fi
cat > "$RT/tools/tar" <<EOF
#!/bin/sh
export LD_LIBRARY_PATH=$RT/toolslibs\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}
exec $RT/tools/tar.real "\$@"
EOF
chmod +x "$RT/tools/tar"

echo "Runtime: $RT"
echo "  lib:      $(ls "$RT/lib" | wc -l) pozycji ($(du -sh "$RT/lib" | cut -f1))"
echo "  lib32:    $(ls "$RT/lib/lib32" | wc -l) pozycji"
echo "  tools:    $(ls "$RT/tools" | tr '\n' ' ')"
echo "  vulkan:   $(ls "$RT/vulkan" | tr '\n' ' ')"
echo "  rootfs:   $(find "$RT/rootfs" -type f | wc -l) plik(ów)"

# --- 7. GPU z runtime'ów flatpaka (uniwersalne: wszystkie sterowniki) -------
# GL.default = glibc-owa Mesa (AMD/Intel hw, llvmpipe sw, zink) — niezależna
# od wersji jądra. GL.nvidia-X-Y-Z = glibc userspace NVIDIA dopasowany przez
# flatpak do modułu w jądrze. Uruchamiać też na maszynie docelowej (musl) —
# sekcja sama pominie brakujące źródła.
for base in /var/lib/flatpak/runtime "$HOME/.local/share/flatpak/runtime"; do
  MS=$(ls -d "$base"/org.freedesktop.Platform.GL.default/x86_64/*/active/files 2>/dev/null | head -1)
  NV=$(ls -d "$base"/org.freedesktop.Platform.GL.nvidia-*/x86_64/*/active/files 2>/dev/null | head -1)
  [ -n "$MS" ] || [ -n "$NV" ] && break
done
if [ -n "${NV:-}" ]; then
  nvver=$(basename "$(dirname "$(dirname "$(dirname "$NV")")")" | sed 's/.*GL\.nvidia-//;s/-/./g')
  find "$RT/lib" -maxdepth 1 -name "*.so.5*" -path '*nvidia*' -delete 2>/dev/null || true
  find "$RT/lib" -maxdepth 1 \( -name 'libnvidia-*' -o -name 'lib*_nvidia.so*' -o -name 'libcuda*' -o -name 'libnvcuvid*' -o -name 'libnvoptix*' \) ! -newer "$NV/lib" -print0 2>/dev/null | xargs -0r rm -f
  cp -a "$NV/lib/." "$RT/lib/" 2>/dev/null || true
  cp -f "$NV"/glvnd/egl_vendor.d/*.json "$RT/lib/egl_vendor.d/" 2>/dev/null || true
  cp -f "$NV"/egl/egl_external_platform.d/*.json "$RT/lib/egl_external/" 2>/dev/null || true
  cp -f "$NV"/vulkan/icd.d/*.json "$RT/vulkan/" 2>/dev/null || true
  echo "  GPU nvidia z flatpaka: $nvver"
fi
if [ -n "${MS:-}" ]; then
  cp -an "$MS"/lib/*.so* "$RT/lib/" 2>/dev/null || true
  mkdir -p "$RT/lib/dri" && cp -a "$MS/lib/dri/." "$RT/lib/dri/" 2>/dev/null || true
  cp -f "$MS"/glvnd/egl_vendor.d/50_mesa.json "$RT/lib/egl_vendor.d/" 2>/dev/null || true
  for j in "$MS"/vulkan/icd.d/*.json; do
    sed "s|/usr/lib/x86_64-linux-gnu/GL/default/lib/|$RT/lib/|g" "$j" > "$RT/vulkan/$(basename "$j")"
  done
  echo "  GPU mesa z flatpaka: $(ls "$RT/lib/dri" | wc -l) sterowników dri"
fi

# --- 8. Helpery WebKitGTK (Epic i inne appki Tauri/WebKit) ------------------
# WebKit spawnuje procesy po zaszytej ścieżce /usr/libexec/webkit2gtk-4.1;
# glibc-run dokłada ją w przestrzeni (overlay lub syntetyczny /usr). Tu tylko
# kopiujemy helpery do runtime'u i przestawiamy interpreter na standardowy.
EPLX=/home/user/.git/epic-musl/app/glibc
if [ -d "$EPLX/libexec" ]; then
  mkdir -p "$RT/libexec/webkit2gtk-4.1" "$RT/lib/webkit2gtk-4.1/injected-bundle"
  cp -f "$EPLX"/libexec/WebKit*Process "$RT/libexec/webkit2gtk-4.1/"
  cp -f "$EPLX"/webkit/injected-bundle/*.so "$RT/lib/webkit2gtk-4.1/injected-bundle/" 2>/dev/null || true
  if command -v patchelf >/dev/null 2>&1; then
    for h in "$RT"/libexec/webkit2gtk-4.1/WebKit*; do
      patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 --remove-rpath "$h" 2>/dev/null || true
    done
  fi
  echo "  webkit helpery: $(ls "$RT/libexec/webkit2gtk-4.1" | tr '\n' ' ')"
fi
