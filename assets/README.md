# Brand assets

- `logo.svg` — original logo with its paper-coloured square background. Used as
  the source for the app icon.
- `logo-transparent.svg` — same artwork with the background removed. Source for
  the transparent overlay exports below.
- `logo-overlay.png` — 1024×1024 still PNG, transparent background.
- `logo-overlay.apng` — 800×800 animated PNG, transparent background, 72 frames
  at 24 fps, loops forever. Best quality; use where APNG is supported.
- `logo-overlay.gif` — 512×512 animated GIF with 1-bit transparency, 60 frames
  at 20 fps, loops forever. Compatibility fallback for editors that cannot
  import APNG.

The animation is a short ease-in pop followed by a gentle hand-drawn wobble and
fade to transparent, so it can be dropped straight onto a video timeline.

Regenerate from `logo-transparent.svg` with ImageMagick and ffmpeg; no downloads
or extra tools are required.
