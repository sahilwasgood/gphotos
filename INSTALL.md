# Install

## Quick install (Linux, no sudo)

```bash
git clone https://github.com/sahilwasgood/gphotos.git
cd gphotos
./install.sh
gphotos
```

This builds the release binary and installs it to `~/.local/bin`, the helpers to
`~/.local/share/gphotos-app`, and the desktop entry and icon into the matching
XDG user directories. Set `GPHOTOS_ROOT` before running it to change the data
directory.

## Manual / system-wide install

Requires Git (to clone from GitHub), Rust (stable), `curl`, `ffmpeg`, Python 3,
and a browser signed in to Google Photos. Without Git, download the source ZIP
from `https://github.com/sahilwasgood/gphotos` and unzip it instead.

```bash
git clone https://github.com/sahilwasgood/gphotos.git
cd gphotos
cargo build --release
```

The binary is `target/release/gphotos`. Copy the scripts from `companion/` into
your config directory (see `companion/README.md`) before first run.

## Linux desktop entry

For a system-wide install:

```bash
sudo install -Dm755 target/release/gphotos /usr/local/bin/gphotos
sudo install -Dm644 gphotos.desktop /usr/share/applications/gphotos.desktop
sudo install -Dm644 icons/hicolor/256x256/apps/gphotos.png \
  /usr/share/icons/hicolor/256x256/apps/gphotos.png
sudo update-desktop-database
```

For a user-only install, copy the same files into
`~/.local/bin/`, `~/.local/share/applications/` and
`~/.local/share/icons/hicolor/256x256/apps/`, then run
`update-desktop-database ~/.local/share/applications`.

The `.desktop` entry sets the icon (from `icons/`), the name **Google Photos**,
and registers the `gphotos://` link scheme. The executable is `gphotos`.

## Windows and macOS

No installer is bundled yet. Build with `cargo build --release` and run the
binary. The window icon is embedded in the executable on all platforms.
