# Companion files

The app expects a few helper files in `GPHOTOS_ROOT` (default
`~/.local/share/gphotos-app`). Copy the two Python scripts from this folder there:

```bash
mkdir -p "${GPHOTOS_ROOT:-$HOME/.local/share/gphotos-app}"
cp companion/gcookies.py companion/fifecookies.py "${GPHOTOS_ROOT:-$HOME/.local/share/gphotos-app}/"
```

They read your own browser cookie stores locally and print cookie headers to
stdout; they never send anything over the network. No cookie value is stored in
this repository.

Python 3 is required, plus these modules on Linux:

```bash
python3 -m pip install pycryptodome secretstorage
```

- `gcookies.py` — finds the signed-in Google session in Firefox / Brave /
  Chrome / Chromium. Used by the app's sign-in flow.
- `fifecookies.py` — adds the fife-scoped cookies that the image CDN needs;
  without it thumbnails, full images and video URLs return 403.

The remaining companion files come from the upstream
[GoogleUnlimited](https://github.com/shishir0x/GoogleUnlimited) project:

- `google-unlimited.sh` — background watch/upload script (needed for automatic
  upload; browsing and management in this app work without it).
- `gotohp-cli_amd64` — uploader binary used only by the "make free" workflow.
  Point `GPHOTOS_GOTOHP` at it if you put it somewhere else.

Copy those from your GoogleUnlimited checkout into `GPHOTOS_ROOT` too if you
need those workflows. The app itself builds and runs with Cargo alone.
