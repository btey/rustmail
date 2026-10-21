#!/usr/bin/env bash
# Cross-compile the Windows x64 executable from Linux for local testing.
# Produces the same layout as the release workflow's portable ZIP:
#   target/windows/rustmail/          (run rustmail.exe from here)
#   target/windows/rustmail-windows-x64.zip
#
# One-time setup:
#   sudo apt install clang lld llvm
#   rustup target add x86_64-pc-windows-msvc
#   cargo install --locked cargo-xwin
# cargo-xwin downloads the Microsoft CRT and Windows SDK on first use; doing so
# accepts Microsoft's license terms for those files.
set -euo pipefail

project_dir="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
target=x86_64-pc-windows-msvc
build_dir="$project_dir/target/windows"
package_dir="$build_dir/rustmail"
zip_path="$build_dir/rustmail-windows-x64.zip"

# Debian and Ubuntu install clang-cl, lld-link, llvm-lib and llvm-rc only
# under the versioned LLVM prefix.
if ! command -v clang-cl >/dev/null 2>&1 || ! command -v llvm-rc >/dev/null 2>&1; then
  llvm_bin="$(ls -d /usr/lib/llvm-*/bin 2>/dev/null | sort -V | tail -n 1 || true)"
  if [[ -n "$llvm_bin" ]]; then
    export PATH="$llvm_bin:$PATH"
  fi
fi
for tool in cargo-xwin clang-cl lld-link llvm-lib llvm-rc; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf '%s is missing. See the setup steps at the top of %s.\n' "$tool" "$0" >&2
    exit 1
  fi
done

# Match the release workflow: link the C runtime statically so Windows needs no
# VC++ redistributable. RUSTFLAGS replaces .cargo/config.toml, so retain its
# stack setting.
export RUSTFLAGS="-C target-feature=+crt-static -C link-arg=/STACK:8000000"
build_args=(--locked --release --target "$target" --bin rustmail --manifest-path "$project_dir/Cargo.toml" --no-default-features --features remote-content,gpu-renderer)
if [[ -n "${RUSTMAIL_APP_FEATURES:-}" ]]; then
  build_args+=(--features "$RUSTMAIL_APP_FEATURES")
fi
if [[ "${RUSTMAIL_SKIP_BUILD:-0}" != "1" ]]; then
  # Extra arguments are passed to Cargo, for example -j 4.
  cargo xwin build "${build_args[@]}" "$@"
fi

executable="$project_dir/target/$target/release/rustmail.exe"
if [[ ! -f "$executable" ]]; then
  printf 'The Windows executable was not built: %s\n' "$executable" >&2
  exit 1
fi

rm -rf "$package_dir"
mkdir -p "$package_dir/LICENSES/Google Sans Flex" "$package_dir/LICENSES/Noto Emoji"
python3 "$project_dir/scripts/stage-pdfium.py" win-x64 "$package_dir" --cache "$build_dir/pdfium-cache"
cp "$executable" "$project_dir"/{README.md,LICENSE,THIRD_PARTY_NOTICES.md} "$package_dir/"
cp -R "$project_dir"/LICENSES/. "$package_dir/LICENSES/"
cp "$project_dir"/resources/fonts/google-sans-flex/{OFL.txt,README.md} "$package_dir/LICENSES/Google Sans Flex/"
cp "$project_dir"/resources/fonts/noto-emoji/{OFL.txt,README.md} "$package_dir/LICENSES/Noto Emoji/"

rm -f "$zip_path"
(cd "$package_dir" && python3 -m zipfile -c "$zip_path" *)
printf 'Windows build ready:\n  %s\n  %s\n' "$package_dir/rustmail.exe" "$zip_path"
