#!/bin/bash
# Rebuild libvirglrenderer-1.dll with venus enabled, msys2 mingw64.
# Invoke from Git Bash with an ABSOLUTE path (CHERE_INVOKE does not resolve a
# relative script path):
#   MSYSTEM=MINGW64 CHERE_INVOKE=1 /c/msys64/usr/bin/bash -l /abs/path/build.sh
# Set TMP/TEMP to a Windows-form msys path first (meson/ninja need it):
#   export TMP=C:/msys64/tmp TEMP=C:/msys64/tmp
#
# Result (measured 2026-09-03, see docs/bench-2026-09-03.md third pass): the
# DLL builds, exports are byte-identical to stock, +1.1 MB of venus code --
# and it still cannot serve venus=on on this host (Blocker A: render server
# is POSIX-only; Blocker B: venus's memory export is fd/dma_buf-only, this
# GPU's Windows driver only exposes VK_KHR_external_memory_win32). This
# script is kept so the build can be reproduced in minutes if upstream ever
# lands an in-process venus path or a Win32-handle memory export -- it need
# not be re-run to get today's (negative) result again.
set -e
set -x

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="${1:-$HERE/_build}"
mkdir -p "$WORK"
cd "$WORK"

# 1. Deps. mingw-w64-x86_64-python-yaml is NOT in the brief's original list --
#    without it, meson dies at src/gallium/meson.build:72:16 ("python3 is
#    missing modules: yaml"); it is in the PKGBUILD's makedepends.
pacman -S --noconfirm --needed \
  mingw-w64-x86_64-vulkan-headers mingw-w64-x86_64-vulkan-loader \
  mingw-w64-x86_64-vulkan-utility-libraries mingw-w64-x86_64-meson \
  mingw-w64-x86_64-ninja mingw-w64-x86_64-python \
  mingw-w64-x86_64-python-yaml mingw-w64-x86_64-libepoxy \
  mingw-w64-x86_64-pkgconf git

# 2. Source: tag 1.3.0 -- matches the installed mingw-w64-virglrenderer
#    package (pacman -Q mingw-w64-x86_64-virglrenderer) on this box.
if [ ! -d virglrenderer ]; then
  git clone --depth 1 -b 1.3.0 https://gitlab.freedesktop.org/virgl/virglrenderer.git
fi
cd virglrenderer

# 3. Apply ONLY venus-mingw-1.3.0.patch -- it already folds in both msys2
#    fixes (001-void-param.patch, 002-no-ioccom.patch) plus the extra
#    Windows ports this task needed (render-server-off-on-Windows +
#    getpagesize/mman/dlfcn/setpriority/thrd_current shims). 001/002 are
#    kept in this dir for reference only; do NOT apply them separately --
#    doing so before the folded patch double-applies both hunks and fails.
git apply "$HERE/venus-mingw-1.3.0.patch"

# 4. Configure. NO -Dplatforms= -- see the trap list in README.md, it
#    silently drops 18 virgl_egl_* exports the existing GLES/D3D11 path uses.
#    -Dvulkan-dload=false makes vkr_library.c use dependency('vulkan')
#    (pkg-config, -lvulkan-1) instead of dlopen("libvulkan.so.1"), so the
#    loader search order picks up C:\Windows\System32\vulkan-1.dll (NVIDIA).
rm -rf build "$WORK/out"
meson setup build \
  -Dvenus=true \
  -Dvulkan-dload=false \
  -Dtests=false \
  --buildtype=release \
  --wrap-mode=nodownload \
  --prefix="$WORK/out"

# 5. Build + install.
ninja -C build
ninja -C build install

NEW="$WORK/out/bin/libvirglrenderer-1.dll"
STOCK="${STOCK_DLL:-/c/qemu-omni-next/libvirglrenderer-1.dll}"

# 6. The discriminating checks. `strings <dll> | grep -ic venus` is
#    DEGENERATE (see README.md) -- it returns 1 for the stock, venus-less
#    DLL too (one "failed to initialize venus renderer" literal compiled
#    into src/virglrenderer.c regardless of ENABLE_VENUS). Use these instead:
echo "=== size (new should be ~1.1 MB larger) ==="
ls -l "$NEW" "$STOCK" 2>/dev/null || ls -l "$NEW"

echo "=== vulkan-1.dll import (0 stock, 1 new) ==="
n=$(objdump -p "$NEW" | grep -c vulkan-1.dll || true)
echo "vulkan-1.dll import count: $n"
[ "$n" -ge 1 ] || { echo "FAIL: no vulkan-1.dll import in $NEW -- not a venus build"; exit 1; }

echo "=== exports: new vs stock (must be IDENTICAL) ==="
objdump -p "$NEW" | awk '/base\[/ && $NF ~ /^virgl_/ {print $NF}' | sort -u > "$WORK/exp-new.txt"
wc -l < "$WORK/exp-new.txt"
if [ -f "$STOCK" ]; then
  objdump -p "$STOCK" | awk '/base\[/ && $NF ~ /^virgl_/ {print $NF}' | sort -u > "$WORK/exp-stock.txt"
  wc -l < "$WORK/exp-stock.txt"
  if diff "$WORK/exp-stock.txt" "$WORK/exp-new.txt"; then
    echo IDENTICAL_EXPORT_SET
  else
    echo "FAIL: export set differs from stock (see diff above) -- $WORK/exp-stock.txt vs $WORK/exp-new.txt"
    exit 1
  fi
fi

echo "=== config.h ==="
grep -iE "VENUS|RENDER_SERVER|VULKAN|EGL" build/config.h

echo BUILD_DONE
