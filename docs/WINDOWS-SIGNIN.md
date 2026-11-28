# Signing in on Windows (ABE fallbacks)

This client normally signs in by reading the Google session cookies out of your
own browser profile. On Linux that works because the cookie store can be
decrypted with the OS keyring.

On Windows, Chrome, Edge and Brave **127+** use **App-Bound Encryption (ABE)**.
The cookie key is bound to the browser executable and the logged-in Windows
account, so an outside process cannot decrypt that store. The bundled
`gcookies.py` helper now knows the Windows profile paths (`%LOCALAPPDATA%` for
Chromium-family browsers, `%APPDATA%` for Firefox) and emits a clear stderr
notice when it meets an ABE-protected jar. It deliberately does **not** try to
defeat ABE. On those machines, automatic cookie decryption will find **no
accounts**. This is a platform limitation, not a sign-in bug.

Two supported fallbacks are provided. Your choice is a product decision; the
app does not pick one automatically.

| | Fallback A — dedicated browser profile | Fallback B — cookie export file |
|---|---|---|
| What it is | A real browser launched normally with `--user-data-dir`, signed in once | A small extension you run writes a local JSON file; the app imports it |
| Automation flags | **None** — no `--remote-debugging-port`, no CDP | None |
| Best for | Keeping app sign-in out of your everyday browser profile | ABE Windows builds where the store cannot be read externally |
| Bundled? | No browser is bundled; you launch your installed browser | No extension is bundled; you install/author a tiny one (format below) |

## Fallback A — app-owned dedicated browser profile

The app discovers identities from *real* browser profiles. The bundled
`gcookies.py` helper globs each OS's native roots: `~/.config` and
`~/.mozilla/firefox` on Linux, `%LOCALAPPDATA%`/`%APPDATA%` on Windows, and
`~/Library/Application Support` on macOS. A profile you launch with
`--user-data-dir` is an ordinary profile, but a *dedicated* folder outside those
roots is not enumerated, so **rescan will not find it**. It is deliberately
**not** driven over the DevTools/CDP port: automation-controlled browsers are
not a supported sign-in path.

1. Pick a folder the app does not use for anything else, e.g.
   `%LOCALAPPDATA%\gphotos-app\browser-profile`.
2. Launch an installed Chromium/Brave browser normally with that folder as its
   user-data-dir, for example:

   ```text
   "C:\Program Files\Google\Chrome\Application\chrome.exe" --user-data-dir="C:\Users\you\AppData\Local\gphotos-app\browser-profile" https://accounts.google.com/AddSession
   ```

   Do **not** add `--remote-debugging-port` or any other automation flag.
3. Sign in to the Google account you want the app to use, then close the browser
   fully so its cookie database is flushed.
4. On Linux/WSL, open **accounts → rescan**: the profile appears as a
   `browser|profile|...` jar identity; choose it, then **use**. Use
   **+ add account** / **+ create new account** to add more identities to that
   profile. On Windows this step cannot find the profile — use Fallback B.

Limits: a dedicated `--user-data-dir` outside the OS's native roots is not
globbed, and on Windows 127+ ABE prevents external decryption even for a profile
that *is* found. On Windows, use Fallback B for that same dedicated profile
(export from it, then import).

## Fallback B — local cookie-export file

The extension is intentionally tiny and runs entirely on your machine: it reads
cookies from a browser profile you are already signed into (the same-origin
extension API), and writes them to a local file. It never talks to the network
and never sends cookies anywhere but that file.

### File format

A UTF-8 JSON object with an `accounts` array. `version` and `fife_cookie` are
optional; `jar`, `email` and `cookie` are required strings.

```json
{
  "version": 1,
  "accounts": [
    {
      "jar": "imported|Default",
      "email": "you@gmail.com",
      "cookie": "SID=...; HSID=...; SSID=...; APISID=...; SAPISID=...",
      "fife_cookie": "OSID=...; OSIDTOKENID=..."
    }
  ]
}
```

- `jar` is a **stable label you choose**, not a filesystem path. Use something
  like `imported|Default`. Keep it the same on every export so the account keeps
  its identity across imports.
- `email` must be the exact account email — the app checks it against the
  account that answers the session.
- `cookie` is a single-line `name=value; name=value` header for Google. CR/LF and
  other control characters are rejected on import.
- `fife_cookie` (optional) carries the OSID pair used by the photo CDN. Include
  it if you want thumbnails/full images to load; otherwise the app falls back to
  the local helper.

The parser rejects anything malformed: invalid JSON, a missing/empty or
non-array `accounts`, wrong field types, empty fields, an email without `@`,
a cookie without `=`, control characters/newlines, more than 100 accounts, or a
file over 256 KiB. A rejected file produces a generic error with **no file
contents** in it.

### Importing

- **From the UI:** open **accounts**, type the file path into *import cookies
  file*, click **import**, then **rescan**. The imported identity appears in the
  list; select it with **use**.
- **From a terminal (headless):**

  ```text
  gphotos.exe --import-cookies "C:\path\to\cookies.json"
  ```

  Success prints only `imported N account(s) ...`; failure prints a generic
  rejection. No email or cookie value is ever printed.

Import writes the cookies into the app's `0600` session cache under `jar|email`
and adds the identity to the account registry. After that, selection and all
photo operations use the same code path as a Linux-discovered account. Re-export
and re-import when Google rotates the session (the old file becomes stale).

## Verification status — be honest about this

- **Verified on Linux** (synthetic files): the format parser, rejection of every
  malformed/unsafe shape listed above, the CLI and UI import hooks, `0600`
  permissions on the files the app writes, and that neither stdout nor stderr
  contains cookie values. `cargo test` and `--selftest` cover the parser and the
  surrounding account flow. The OS path mapping (Linux/Windows/macOS), macOS
  PBKDF2 rounds and the content-free ABE notice are covered by fixture tests
  only; no real Windows/macOS browser store was opened.
- **NOT verified here, and not verifiable on Linux:** that ABE behaves as
  described, that any extension successfully reads a Windows browser's cookies,
  that the `--user-data-dir` profile is enumerated on a real Windows install,
  or that Windows file-permission semantics match the Unix `0600` guarantee.
  None of these fallbacks was exercised against a real Windows machine or a real
  ABE-protected cookie store. Treat the setup steps above as a contract to test
  on a real Windows box, not as a confirmed result.

## Security notes

- **No cookie values, emails or tokens in logs, toasts or git.** Errors about a
  bad export are deliberately content-free. Do not paste a real export into an
  issue or commit it.
- **Local file permissions.** The app creates its session cache and registry
  with mode `0600` from the moment the file is created (never world-readable,
  then chmod-ed). On Unix, keep the *export file* owner-only too:
  `chmod 600 cookies.json`. On Windows, put it in a folder only your OS user can
  read (e.g. under `%LOCALAPPDATA%`), and rely on NTFS ACLs — the Unix `0600`
  guarantee does not translate to Windows.
- **Delete the export after importing.** It is a live Google credential in
  plaintext; re-export when needed rather than keeping it around.
- The extension is **not bundled** with this repository. Review any extension
  you install before letting it read your cookies. Only import files you trust.
- Imported accounts are ordinary identities: **hide**/**remove** work as they do
  for discovered accounts, and nothing is saved to the registry until you use
  the import or an explicit Add.

## Known limits

- Browser binary mapping and the account chooser's add/create buttons use the OS
  URL helper on Windows; they do not auto-launch a named `--user-data-dir`
  profile. Launch the dedicated profile yourself (step 2 above).
- `add_jar` (the profile picker in *accounts*) is per-session, not persisted.
- A stale imported cookie fails revalidation; the app will report no session.
  Re-export from a still-signed-in browser profile.
