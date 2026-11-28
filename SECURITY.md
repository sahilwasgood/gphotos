# Security policy

## Scope

This is a desktop client that holds a signed-in Google Photos session for the local OS user.
The threat model is a **single-user desktop**: other local users/processes on the same machine
are considered untrusted; the user's own browser and Google account are trusted inputs.

## What the app does with credentials

- The Google session cookie and CSRF token are read from the local browser's cookie store via
  the companion scripts in `GPHOTOS_ROOT` (see README). They are never sent anywhere except
  `photos.google.com` and `googleusercontent.com`.
- The cached session (`sessions.json` in the OS cache directory) is created with mode `0600`.
- No credential is passed in a process argument list: `curl` receives its configuration (headers,
  URL, POST body) over stdin. The **one exception** is the short-lived signed video URL passed
  to `ffmpeg` as `-i <url>`; ffmpeg has no stdin-config equivalent. That URL expires quickly
  and is scoped to one media item.
- No telemetry, analytics, crash reporting, or auto-update network calls exist in the source.

## Reporting a vulnerability

Please open a private security advisory on this repository rather than a public issue. Include:

- affected version/commit,
- reproduction steps,
- impact (what an attacker could read or change),
- suggested fix if you have one.

This is a small project, so replies may take a little time. Please do not publicly
disclose until a fix is available.

## Known limitations (not vulnerabilities)

- Permanent delete uses a reverse-engineered RPC id; it is gated behind an explicit two-step
  confirmation that names the uncertainty. Trash/restore and archive are the verified paths.
- Sharing "Manage" opens the official Google Photos web page: changing who can view a link uses
  private web RPCs / mobile gRPC and is deliberately not reimplemented here.
- The upstream `google-unlimited.sh` and uploader binary are external to this repository and
  must be reviewed separately before trusting them with a session. The bundled cookie helpers
  (`companion/gcookies.py`, `companion/fifecookies.py`) read local cookie stores only.
