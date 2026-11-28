# Brand assets

Two distinct marks:

- **Icon** — the square picture with the paper-coloured background, used as the
  desktop/window icon. Source: `icon.svg`; embedded copy: `src/icon.png`;
  installed copy: `icons/hicolor/256x256/apps/gphotos.png`.
- **Logo** — the hand-drawn mark drawn by `draw_logo()` in `src/main.rs`, which
  sits next to the "Google Photos" title in the app's top-left corner. It has no
  square background. The files here are the app's own logo, not the icon.

## Transparent logo exports

- `logo.png` — the native in-app logo, 936×1008, transparent background.
- `logo.apng` — the same logo with its **native animation**: sun rays rotate at
  0.35 rad/s, the cloud drifts at 0.9 rad/s and bobs at 2.2 rad/s. 160 frames at
  20 fps (8 seconds), transparent, loops.
- `logo.gif` — 468×504 transparency fallback for editors that cannot import APNG.
- `render_logo.py` — ports `draw_logo()` and the sketchy drawing helpers exactly,
  so the exports can be regenerated from the same code path. Needs Pillow
  (`python3 -m pip install pillow`) and ffmpeg to assemble the animation:

  ```bash
  python3 assets/render_logo.py /tmp/logo-frames 8 20
  cp /tmp/logo-frames/logo-native.png logo.png
  ffmpeg -y -framerate 20 -i /tmp/logo-frames/f%04d.png \
    -c:v apng -pix_fmt rgba -plays 0 logo.apng
  ffmpeg -y -framerate 20 -i /tmp/logo-frames/f%04d.png \
    -vf "scale=468:-1,split[s0][s1];[s0]palettegen=reserve_transparent=1[p];[s1][p]paletteuse=alpha_threshold=128" \
    -loop 0 logo.gif
  ```

The animation contains only the motion the app itself has — no added fade,
scale or spin. The APNG/GIF loop is a cut at 8 seconds rather than a seamless
cycle: the three native frequencies share no short common period.
