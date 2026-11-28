# Brand assets

- `logo.svg` — original logo artwork with its paper-coloured square background.
  Source for the app icon (`src/logo.png`, also used by the Linux desktop entry).
- `logo-overlay.png` — the **native in-app logo** from `draw_logo()` in
  `src/main.rs`, 936×1008, transparent background.
- `logo-overlay.apng` — the same logo with its **native animation**: sun rays
  rotate at 0.35 rad/s, the cloud drifts at 0.9 rad/s and bobs at 2.2 rad/s.
  160 frames at 20 fps (8 seconds), transparent, loops.
- `logo-overlay.gif` — 468×504 transparency fallback for editors that cannot
  import APNG, same motion.
- `render_logo.py` — ports `draw_logo()` and the sketchy drawing helpers
  exactly, so the overlays can be regenerated from the same code path:

  ```bash
  python3 assets/render_logo.py /tmp/logo-frames 8 20
  ```

The animation contains only the motion the app itself has — no added fade,
scale or spin. The APNG/GIF loop is a cut at 8 seconds, not a perfectly seamless
cycle: the app's ray, drift and bob frequencies share no short common period.
