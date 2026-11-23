# Known limitations

Honest notes about things this client does not do, so you can decide if it fits
your needs. No feature is faked to look complete.

1. **People & Pets** — the app does not show Google's face groups. There is no
   published, reliable web endpoint for them; implementing it would need a live
   capture of the browser's private requests.
2. **Sharing "Manage"** — link access and collaborators are managed on the web.
   The menu opens the album or link page in your browser; changing those
   settings through private web RPCs is not supported.
3. **Permanent delete** — uses a reverse-engineered request. It is available
   but guarded by a two-step confirmation that says so explicitly.
4. **A/V sync** — audio is fed to the sound server and its byte count is used
   as the clock. A small constant buffer latency (tens of milliseconds)
   remains; there is no true PTS-based synchronisation.
5. **Streaming high-bitrate video** — a 15 s read timeout is used. If the first
   frame never arrives the player shows "could not load video — click to
   retry". CDNs that reject range requests are downloaded to a local cache
   first (up to 1800 s).
6. **Local views** (videos, large files, duplicate candidates, "On this day",
   type views, combinable filters) use the one-time library scan. They show an
   honest "needs the one-time scan" message until it completes.
7. **Sorting in local views** — "oldest", "name" and "largest" sort the items
   currently loaded in that view; paging appends in chronological order.
8. **Colab batch job** — signed URLs are written to a local `urls.tsv` and
   pasted into the notebook manually. They expire, so regenerate the job right
   before running it. `urls.tsv` is never part of the uploaded bundle.
