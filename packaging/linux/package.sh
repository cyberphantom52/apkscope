#!/bin/bash
# Package the Linux viewer build as a .deb and an AppImage.
#
# usage: packaging/linux/package.sh <apkscope-binary> <output-dir>
#
# The viewer links nothing beyond libc at build time: X11, Wayland, GL/EGL and
# the xdg-desktop-portal file dialogs are dlopen()'d at runtime. So the .deb just
# declares the runtime libraries as Depends, and the AppImage bundles only the
# binary, the desktop file and the icon (no libraries, no GL driver conflicts).
# The raw binary itself is shipped by the caller (release.yml / ci.yml copy it
# to apkscope_linux_<arch>), so it is not duplicated here.
#
# appimagetool is fetched from the AppImage project's continuous release unless
# APPIMAGETOOL points at an existing one.

set -euo pipefail

bin=$(realpath "$1")
out=$2
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")

case "$(uname -m)" in
    x86_64)  arch_app=x86_64 ;;
    aarch64) arch_app=aarch64 ;;
    *) echo "error: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
arch_deb=$(dpkg --print-architecture 2>/dev/null || case "$arch_app" in x86_64) echo amd64 ;; aarch64) echo arm64 ;; esac)

[ -x "$bin" ] || { echo "error: binary not found or not executable: $bin" >&2; exit 1; }
mkdir -p "$out"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# ---------- .deb ----------
debroot=$work/deb
mkdir -p "$debroot/DEBIAN" \
         "$debroot/usr/bin" \
         "$debroot/usr/share/applications" \
         "$debroot/usr/share/icons/hicolor/256x256/apps" \
         "$debroot/usr/share/doc/apkscope"
install -m 755 "$bin" "$debroot/usr/bin/apkscope"
install -m 644 "$root/packaging/linux/io.github.suffixing.apkscope.desktop" "$debroot/usr/share/applications/"
install -m 644 "$root/assets/logo-256.png" "$debroot/usr/share/icons/hicolor/256x256/apps/apkscope.png"
install -m 644 "$root/LICENSE" "$debroot/usr/share/doc/apkscope/copyright"
install -m 644 "$root/NOTICE" "$debroot/usr/share/doc/apkscope/"

# AppStream metadata: __VERSION__/__DATE__ filled in from this build, so the
# <releases> entry never goes stale. Software centers read it, and appimagetool
# warns when an AppImage ships without it.
mkdir -p "$debroot/usr/share/metainfo"
sed -e "s/__VERSION__/$version/" -e "s/__DATE__/$(date -u +%F)/" \
    "$root/packaging/linux/io.github.suffixing.apkscope.appdata.xml" > "$debroot/usr/share/metainfo/io.github.suffixing.apkscope.appdata.xml"

# dlopen()'d runtime libraries (see ldd: nothing beyond libc is linked)
cat > "$debroot/DEBIAN/control" <<EOF
Package: apkscope
Version: $version
Section: devel
Priority: optional
Architecture: $arch_deb
Depends: libc6, libx11-6, libxcb1, libxcursor1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1, libgl1, libegl1
Recommends: xdg-desktop-portal
Maintainer: suffixing <suffixing@users.noreply.github.com>
Homepage: https://github.com/suffixing/apkscope
Installed-Size: $(du -sk "$debroot/usr" | cut -f1)
Description: Android APK decompiler, xref engine and MCP server for AI agents
 Readable Java, API recon, Gradle export, APK re-signing and a native
 viewer, in one small binary. The viewer uses the desktop's X11 or
 Wayland stack; file dialogs go through xdg-desktop-portal.
EOF

for script in postinst postrm; do
    cat > "$debroot/DEBIAN/$script" <<'EOF'
#!/bin/sh
# refresh the desktop-entry and icon caches when the tools are present
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
exit 0
EOF
    chmod 755 "$debroot/DEBIAN/$script"
done

deb="$out/apkscope_${version}_${arch_deb}.deb"
# xz instead of the dpkg default zstd: installable on Ubuntu 20.04 / Debian 11 too
dpkg-deb --root-owner-group --build -Zxz "$debroot" "$deb"
echo "built $deb"

# ---------- AppImage ----------
appdir=$work/AppDir
mkdir -p "$appdir/usr/bin"
install -m 755 "$bin" "$appdir/usr/bin/apkscope"
install -m 755 "$root/packaging/linux/AppRun" "$appdir/AppRun"
# the desktop file lives at the AppDir root (where appimagetool reads it) and
# in usr/share/applications/ (where appstreamcli validate-tree resolves the
# appdata's launchable desktop-id from)
mkdir -p "$appdir/usr/share/applications"
install -m 644 "$root/packaging/linux/io.github.suffixing.apkscope.desktop" "$appdir/"
install -m 644 "$root/packaging/linux/io.github.suffixing.apkscope.desktop" "$appdir/usr/share/applications/"
install -m 644 "$root/assets/logo-256.png" "$appdir/apkscope.png"
mkdir -p "$appdir/usr/share/metainfo"
install -m 644 "$debroot/usr/share/metainfo/io.github.suffixing.apkscope.appdata.xml" "$appdir/usr/share/metainfo/"

tool=${APPIMAGETOOL:-$work/appimagetool}
if [ ! -x "$tool" ]; then
    url="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${arch_app}.AppImage"
    echo "fetching $url"
    curl -fL --retry 3 -o "$tool" "$url"
    chmod +x "$tool"
fi
appimage="$out/apkscope-${version}-${arch_app}.AppImage"
# APPIMAGE_EXTRACT_AND_RUN: hosted runners (and minimal CI containers) have no FUSE
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=$arch_app "$tool" "$appdir" "$appimage"
echo "built $appimage"
