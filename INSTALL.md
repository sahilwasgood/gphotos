# Install

## Build from source

Requires Rust (stable), `curl`, `ffmpeg`, Python 3, and a browser signed in to
Google Photos.

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

The `.desktop` entry sets the icon, the name **gphotos**, and registers the
`gphotos://` link scheme.

## Windows and macOS

No installer is bundled yet. Build with `cargo build --release` and run the
binary. The window icon is embedded in the executable on all platforms.
