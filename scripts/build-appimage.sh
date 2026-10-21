#!/usr/bin/env bash
set -euo pipefail

project_dir="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
build_dir="$project_dir/target/appimage"
app_dir="$build_dir/rustmail.AppDir"
output_path="$build_dir/rustmail.AppImage"
staged_output="$build_dir/.rustmail.AppImage.new"
tools_dir="$build_dir/tools"

download_tool() {
  local url="$1"
  local destination="$2"
  if [[ -x "$destination" ]]; then
    return
  fi
  mkdir -p "$(dirname "$destination")"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --silent --show-error "$url" --output "$destination.new"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$destination.new"
  else
    printf 'curl or wget is required to obtain the AppImage deployment tools.\n' >&2
    exit 1
  fi
  chmod +x "$destination.new"
  mv -f "$destination.new" "$destination"
}

mkdir -p "$build_dir"
# Include both renderer choices; CPU remains the initial runtime selection.
# RUSTMAIL_APP_FEATURES can enable additional application features.
app_features="${RUSTMAIL_APP_FEATURES:-}"
build_args=(--locked --bin rustmail --manifest-path "$project_dir/Cargo.toml" --release --no-default-features --features remote-content,gpu-renderer)
if [[ -n "$app_features" ]]; then
  build_args+=(--features "$app_features")
fi
# The controls are app-owned and the Linux shell uses winit directly.
if [[ "${RUSTMAIL_SKIP_BUILD:-0}" == "1" ]]; then
  if [[ ! -x "$project_dir/target/release/rustmail" ]]; then
    printf 'RUSTMAIL_SKIP_BUILD=1 requires an existing release executable.\n' >&2
    exit 1
  fi
else
  cargo build "${build_args[@]}"
fi

if ldd "$project_dir/target/release/rustmail" | grep -q 'libQt'; then
  printf 'The release binary unexpectedly links Qt; refusing to package it.\n' >&2
  exit 1
fi

rm -rf "$app_dir"
mkdir -p \
  "$app_dir/usr/bin" \
  "$app_dir/usr/share/applications" \
  "$app_dir/usr/share/doc/rustmail/google-sans-flex" \
  "$app_dir/usr/share/icons/hicolor/512x512/apps" \
  "$app_dir/usr/share/icons/hicolor/scalable/apps" \
  "$app_dir/usr/share/metainfo"
cp "$project_dir/target/release/rustmail" "$app_dir/usr/bin/rustmail"
python3 "$project_dir/scripts/stage-pdfium.py" linux-x64 "$app_dir/usr/lib/rustmail"
app_version="$(python3 "$project_dir/scripts/stage-linux-metadata.py" "$app_dir")"
cp "$project_dir/resources/app-icon/rustmail-masked-512.png" "$app_dir/usr/share/icons/hicolor/512x512/apps/org.rustmail.mail.png"
cp "$project_dir/resources/app-icon/rustmail-masked.svg" "$app_dir/usr/share/icons/hicolor/scalable/apps/org.rustmail.mail.svg"
cp "$project_dir/LICENSE" "$app_dir/usr/share/doc/rustmail/LICENSE"
cp -R "$project_dir/LICENSES" "$app_dir/usr/share/doc/rustmail/LICENSES"
cp "$project_dir/THIRD_PARTY_NOTICES.md" "$app_dir/usr/share/doc/rustmail/THIRD_PARTY_NOTICES.md"
cp "$project_dir/resources/fonts/google-sans-flex/OFL.txt" \
  "$app_dir/usr/share/doc/rustmail/google-sans-flex/OFL.txt"
cp "$project_dir/resources/fonts/google-sans-flex/README.md" \
  "$app_dir/usr/share/doc/rustmail/google-sans-flex/README.md"
cp "$project_dir/resources/AppRun" "$app_dir/AppRun"
chmod +x "$app_dir/AppRun" "$app_dir/usr/bin/rustmail"

linuxdeploy="${LINUXDEPLOY:-$tools_dir/linuxdeploy-x86_64.AppImage}"
download_tool \
  'https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage' \
  "$linuxdeploy"
appimagetool="${APPIMAGETOOL:-$tools_dir/appimagetool-x86_64.AppImage}"
download_tool \
  'https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage' \
  "$appimagetool"

# Winit opens libxkbcommon-x11 with dlopen(), so linuxdeploy cannot discover
# it from the executable. Hosts without it (Ubuntu without Qt, minimal X11
# sessions such as the AppImageHub test runner) panic before the first window.
# It is not on the AppImage excludelist; bundle it with its xkbcommon/xcb-xkb
# dependencies, like the deb and rpm packages declare it.
xkbcommon_x11_dir="$(pkg-config --variable=libdir xkbcommon-x11)"
xkbcommon_x11="$xkbcommon_x11_dir/libxkbcommon-x11.so.0"
if [[ ! -f "$xkbcommon_x11" ]]; then
  printf 'libxkbcommon-x11.so.0 was not found; install libxkbcommon-x11-dev.\n' >&2
  exit 1
fi

# Deploy ordinary ELF dependencies. Winit uses the host Wayland/X11 client
# libraries through its normal backend discovery rather than a toolkit plugin.
VERSION="$app_version" APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}" \
  "$linuxdeploy" \
  --appdir "$app_dir" \
  --executable "$app_dir/usr/bin/rustmail" \
  --library "$xkbcommon_x11"
for library in libxkbcommon-x11.so.0 libxkbcommon.so.0; do
  if [[ ! -e "$app_dir/usr/lib/$library" ]]; then
    printf 'linuxdeploy did not bundle %s.\n' "$library" >&2
    exit 1
  fi
done

# Preserve the allocator tuning and stable process name from our launcher.
cp "$project_dir/resources/AppRun" "$app_dir/AppRun"
chmod +x "$app_dir/AppRun"

# The pinned PDFium runtime depends only on the ordinary glibc runtime. Keep
# its private path intact and verify it after deployment has adjusted the ELF files.
python3 "$project_dir/scripts/test-pdf-preview.py" "$app_dir/usr/bin/rustmail"

# AppDir specifies a PNG .DirIcon for file-manager thumbnails. linuxdeploy
# prefers the scalable icon for the root entry, so restore the PNG thumbnail
# explicitly while retaining the SVG and themed icon copies for integration.
cp "$project_dir/resources/app-icon/rustmail-masked-512.png" "$app_dir/rustmail.png"
ln -sfn rustmail.png "$app_dir/.DirIcon"

mkdir -p "$(dirname "$output_path")"
rm -f "$staged_output"
VERSION="$app_version" APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}" \
  "$appimagetool" "$app_dir" "$staged_output"
# Renaming over an executing AppImage is atomic on Linux; the running process
# keeps its old inode while new launches receive the freshly packaged build.
mv -f "$staged_output" "$output_path"

printf 'Built %s\n' "$output_path"
