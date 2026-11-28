# gphotos

A native desktop client for Google Photos. Built in Rust with egui, it opens your
library in a hand-drawn paper-and-ink timeline, plays videos in-app, and lets you
manage photos without a browser tab open.

It uses a **session bridge**: small local helpers read your own signed-in Google
session, and the app talks to the same private web RPC the Photos website uses.
No embedded browser, no re-uploading just to browse, no telemetry.

> Work in progress. Features that are not implemented are listed in
> [LIMITATIONS.md](LIMITATIONS.md) rather than faked.

## Install

### 1. Requirements

- Rust (stable) and `curl`
- `ffmpeg` (for video playback)
- Python 3 — on Linux also `pycryptodome` and `secretstorage`:

  ```bash
  python3 -m pip install pycryptodome secretstorage
  ```

- A browser on this machine signed in to Google Photos (Brave, Chrome, Firefox
  or Chromium). Video sound needs a 48 kHz stereo output device; without one,
  videos play silently and the picture keeps time.

### 2. Build and run

```bash
git clone https://github.com/sahilwasgood/gphotos.git
cd gphotos
cargo build --release
mkdir -p "${GPHOTOS_ROOT:-$HOME/.local/share/gphotos-app}"
cp companion/gcookies.py companion/fifecookies.py "${GPHOTOS_ROOT:-$HOME/.local/share/gphotos-app}/"
./target/release/gphotos
```

On first run, use the in-app sign-in flow and confirm in the browser. Nothing is
written into the repository. See [INSTALL.md](INSTALL.md) for Linux desktop
integration (`.desktop` entry, icon and `gphotos://` link handler), and
[docs/WINDOWS-SIGNIN.md](docs/WINDOWS-SIGNIN.md) if you are on Windows or macOS,
where automatic cookie decryption is not available.

There is no packaged installer yet — the release binary is the install.

## What you can do

- **Timeline** with a month rail sized by items per month; click or drag to jump, double-click a month to filter to that year.
- **Video playback** in-app: streaming with seek, hover previews, per-file resume and mute.
- **Search** via Google plus `file:` search over filenames and captions; combinable local filters (videos, large files, duplicates, bursts, year chips).
- **Albums**: create, delete, share links, and browse shared albums and links.
- **Storage**: quota, biggest-items report, per-type and per-year breakdown, "make free" workflow.
- **Manage**: multi-select, keyboard triage (F favourite, E archive, Delete trash), undo for bulk actions, caption/date editing, CSV metadata export, original downloads.
- **Accounts**: switch between browser profiles, add accounts, logout, locked folder.
- **Things & places**: real Google groups such as birthdays and cities.
- **AI agents**: a JSON-lines CLI (`gphotos --agent …`) with read commands and gated writes. See [AGENT.md](AGENT.md).

## Limitations worth knowing

- People & Pets face groups are not implemented — no reliable cookie-accessible endpoint exists.
- Sharing access management opens the official web page.
- Permanent delete uses a reverse-engineered RPC and is behind a two-step confirmation.
- Local filters need a one-time library scan before they fill.
- The Linux sync robot / local uploader needs systemd; on other OSes those controls are disabled, not faked.

Full details in [LIMITATIONS.md](LIMITATIONS.md).

## Deep links

Pass a link as the first argument to open it on launch:

```bash
./target/release/gphotos gphotos://photo/AF1QipABC123
./target/release/gphotos gphotos://album/AAK-1234567890
./target/release/gphotos /path/to/shared.gphotoslink
```

A `.gphotoslink` file is plain text whose first non-empty, non-comment line is a
`gphotos://` or `https://photos.google.com/` photo/album URL. Add `?account=<email>`
to select the profile that created the link.

## Configuration

`GPHOTOS_ROOT` overrides the data directory (default `~/.local/share/gphotos-app`);
`GPHOTOS_GOTOHP` points at the optional uploader binary used by "make free".
Uploader and watcher scripts come from
[GoogleUnlimited](https://github.com/shishir0x/GoogleUnlimited) and are only needed
for those workflows. The Python helpers are documented in
[companion/README.md](companion/README.md).

## Tests

```bash
cargo test --bin gphotos              # offline unit tests
./target/release/gphotos --selftest   # offline + live read-only checks (needs sign-in)
```

## Project layout

- `src/main.rs` — UI, video player, CLI and selftest.
- `src/library.rs` — session bridge, `batchexecute` RPC, caching and scans.
- `src/agent.rs` — JSON-lines CLI for AI agents.
- `companion/` — local cookie helpers and their tests.
- `assets/` — the in-app logo (with transparent animated exports) and the icon source.
- Detail docs: `INSTALL.md`, `LIMITATIONS.md`, `SECURITY.md`, `AGENT.md`,
  `ENDPOINT-RESEARCH.md`, `docs/WINDOWS-SIGNIN.md`.

## Security and licence

Credentials never enter the process argument list (curl reads config from stdin);
the one exception is a short-lived signed video URL passed to ffmpeg. No telemetry.
See [SECURITY.md](SECURITY.md) to report a vulnerability.

GPL-3.0 — see [LICENSE](LICENSE). Bundled fonts are under the SIL Open Font
License, see [FONTS-LICENSE.md](FONTS-LICENSE.md) and [OFL.txt](OFL.txt).
