# gphotos-app

A native desktop client for Google Photos, written in Rust with [egui](https://github.com/emilk/egui).
It talks to the same private web endpoints the browser app uses, so media stays in your Google
account and nothing is re-uploaded just to browse it. The UI is intentionally hand-drawn
(Patrick Hand / Caveat) with a paper-and-ink palette.

> Status: actively developed. See `LIMITATIONS.md` for honest limitations and
> `CONTRIBUTING.md` if you want to help.

## Features

- **Timeline** with a month/date rail sized by items per month; click or drag to jump, double-click a month to filter to that year.
- **Video playback** in-app: native ffmpeg pipe, streaming with seek, muted testing hooks, hover previews, per-file resume, proximity-hidden controls.
- **Search** through Google plus local `file:` search over filenames/captions from the one-time library scan.
- **Local filters**: combinable videos/photos/large toggles (e.g. videos ≥100 MB), year chips, month/type views, duplicate candidates, bursts, "On this day"; sortable by date, name or size.
- **Albums**: create, delete, share links, copy link, manage access on the web, sort, paperclip badge on shared albums; dedicated shared-album and shared-link views.
- **Storage**: quota reading, full-library measurement, biggest-items report, per-type/per-year breakdown, make-items-free workflow with optional Colab batch job.
- **Management**: multi-select (Ctrl+A, Ctrl/Shift-click), keyboard triage (F/E/Delete), per-photo right-click actions, undo for bulk trash/archive/favourite, caption and date editing, metadata CSV export, bulk original download with real filenames. Sort preferences persist.
- **Settings**: appearance, playback, uploads, caching, locked-folder visibility, Colab engine — all persisted.
- **Privacy**: session cookies and at-tokens never appear in process argv (curl config over stdin); the signed video URL is the one exception passed to ffmpeg as an argument (see Honest limitations). No telemetry.

## Build and run

Requirements: Rust (stable), `curl`, `ffmpeg`, Python 3 with `pycryptodome` and
`secretstorage` (Linux), and a browser signed in to Google Photos on this machine.
Video sound needs an output device that runs at 48 kHz stereo. If yours cannot,
videos play silently and the picture keeps time. See `companion/README.md` before
first run.

```bash
git clone https://github.com/sahilwasgood/gphotos.git
cd gphotos-app
cargo build --release
./target/release/gphotos-app
```

On first run, use the in-app sign-in flow, then confirm in the browser. The app reads the
existing browser session (Brave/Chrome/Firefox cookie sources via the helper scripts described
below). No credentials are ever written into the repository.

## Configuration

All paths are portable and can be overridden:

| Variable | Meaning | Default |
|---|---|---|
| `GPHOTOS_ROOT` | companion-scripts and config directory | `~/.local/share/gphotos-app` |
| `GPHOTOS_GOTOHP` | uploader binary used by "make free" | `$GPHOTOS_ROOT/gotohp-cli_amd64` |
| `GPHOTOS_MUTE` | force mute (used in automated testing) | unset |

`gcookies.py` and `fifecookies.py` are bundled in `companion/` — copy them into
`GPHOTOS_ROOT` as described in `companion/README.md`. Without them the app cannot
find a browser session or load image/video URLs on a fresh machine.
`google-unlimited.sh` and `gotohp-cli_amd64` come from the upstream
[GoogleUnlimited](https://github.com/shishir0x/GoogleUnlimited) project and are
only needed for the upload / "make free" workflows.

Debug/test hooks: `GPHOTOS_SHOT`, `GPHOTOS_CTXDBG`, `GPHOTOS_SCROLLDBG`, `GPHOTOS_VDBG`,
`GPHOTOS_HOGS`, `GPHOTOS_FORCE_STREAM`, `GPHOTOS_JUMPDATE`/`GPHOTOS_JUMPTEXT`. They are
environment-gated and cost nothing during normal use.

## Tests

```bash
cargo test --bin gphotos-app          # 42 offline unit tests
./target/release/gphotos-app --selftest   # 15 offline + 27 live read-only checks (live needs a signed-in browser session)
```

## Architecture

- `src/main.rs` — UI, state, player, CLI, selftest.
- `src/library.rs` — batchexecute RPC wrapper, response parsers, full-library scan, stats.
- Playback: an ffmpeg rawvideo pipe feeds a single frame slot; a separate ffmpeg→cpal pipe
  counts bytes handed to the audio device as the master clock. Seeking a non-range-capable
  CDN waits for the local download cache, then restarts from disk.

## Honest limitations

- **People & Pets / face groups**: no published batchexecute RPC was found; not implemented rather than faked.
- **Sharing access management** (who can view, expiry, collaborators): the web app uses private RPCs and the mobile app uses gRPC with OAuth; the app opens the official web page for these actions.
- **Permanent delete** uses a reverse-engineered RPC id and is behind a two-step confirmation that says so.
- **Sync robot and local uploader are Linux-only**: automatic folder watching needs a systemd user service, `google-unlimited.sh` and the Linux uploader. On macOS/Windows the Sync robot page shows a "not available on this OS" notice, and the Storage and Uploads cards disable the make-free buttons, the "This computer" choice and the upload button rather than leaving dead controls. Browsing, account switching, video, storage and settings work on every platform (automatic sign-in excepted — see below).
- **macOS automatic sign-in**: automatic cookie decryption needs the Linux keyring; on macOS use the cookie-export import. Once a session exists, switching accounts works normally.
- Signed video URLs appear in the ffmpeg process argument list; ffmpeg has no stdin-config equivalent. They are short-lived and per-item.

## Security

See `SECURITY.md` for the credential-handling model and how to report a vulnerability.

## Licence

GPL-3.0. See `LICENSE`. Fonts: Patrick Hand and Caveat (included in `src/`, see `FONTS-LICENSE.md` and `OFL.txt`).
