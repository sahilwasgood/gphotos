#!/usr/bin/env bash
# One-command user install (no sudo). Builds from source and installs into
# ~/.local/bin plus the XDG user data/applications/icons dirs.
set -euo pipefail

cd "$(dirname "$0")"

PREFIX="${HOME}/.local"
DATA="${GPHOTOS_ROOT:-${HOME}/.local/share/gphotos-app}"

for tool in cargo curl ffmpeg python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "missing required tool: $tool" >&2
    exit 1
  }
done

echo "building gphotos..."
cargo build --release

install -Dm755 target/release/gphotos "${PREFIX}/bin/gphotos"
mkdir -p "${DATA}"
install -m644 companion/gcookies.py companion/fifecookies.py "${DATA}/"
install -Dm644 gphotos.desktop "${PREFIX}/share/applications/gphotos.desktop"
install -Dm644 icons/hicolor/256x256/apps/gphotos.png \
  "${PREFIX}/share/icons/hicolor/256x256/apps/gphotos.png"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${PREFIX}/share/applications" >/dev/null 2>&1 || true
fi

echo "installed to ${PREFIX}/bin/gphotos"
echo "run: gphotos"
case ":${PATH}:" in
  *":${PREFIX}/bin:"*) ;;
  *) echo "note: add ${PREFIX}/bin to your PATH" ;;
esac
