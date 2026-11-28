#!/usr/bin/env python3
"""Render the app's NATIVE logo animation (src/main.rs draw_logo) to transparent PNG frames.

Everything here is a direct port of draw_logo and its sketchy helpers: same
coordinates, colours, deterministic jitter seeds, rotating sun rays and the
bobbing/drifting cloud. No extra fade, scale or spin is added.

Usage: python3 assets/render_logo.py OUT_DIR [SECONDS] [FPS]
"""

import math
import struct
import sys
from pathlib import Path

from PIL import Image, ImageDraw

INK = (0x2B, 0x26, 0x20)
INK_SOFT = (0x8B, 0x7D, 0x6B)
RED = (0xD9, 0x4F, 0x30)
BLUE = (0x3B, 0x6E, 0xA5)
GREEN = (0x4A, 0x7C, 0x59)
BACK = (0xF3, 0xE7, 0xD3)
FRONT = (0xFF, 0xFD, 0xF5)
WHITE = (255, 255, 255)

# The app allocates 78x84 points at scale 1.0 in the top bar.
W, H = 78, 84


def f32(x):
    return struct.unpack("<f", struct.pack("<f", x))[0]


def jitter(seed):
    # Rust: ((seed as f32 * 12.9898).sin() * 43758.5453).fract() * 2.0 - 1.0
    a = f32(f32(float(seed & 0xFFFFFFFF)) * f32(12.9898))
    v = f32(f32(math.sin(a)) * f32(43758.5453))
    frac = f32(v - float(math.trunc(v)))
    return f32(f32(frac * 2.0) - 1.0)


class Canvas:
    def __init__(self, k):
        self.k = k
        self.img = Image.new("RGBA", (W * k, H * k), (0, 0, 0, 0))
        self.d = ImageDraw.Draw(self.img)

    def pt(self, x, y):
        return (x * self.k, y * self.k)

    def poly(self, pts, fill):
        self.d.polygon([self.pt(x, y) for x, y in pts], fill=fill)

    def line(self, a, b, color, width):
        self.lines([a, b], color, width)

    def lines(self, points, color, width):
        w = max(1, round(width * self.k))
        pts = [self.pt(x, y) for x, y in points]
        # egui tessellates a stroke as a continuous path; curve joins and round
        # ends keep the hand-drawn wobble without butt-cap spikes
        self.d.line(pts, fill=color, width=w, joint="curve")
        r = w / 2
        for x, y in (pts[0], pts[-1]):
            self.d.ellipse((x - r, y - r, x + r, y + r), fill=color)


def sketchy_line(c, a, b, color, width, seed):
    dist = math.hypot(b[0] - a[0], b[1] - a[1])
    segs = max(math.ceil(dist / 14.0), 2)
    dx, dy = b[0] - a[0], b[1] - a[1]
    length = max(dist, 1.0)
    px, py = -dy / length, dx / length
    points = [a]
    for i in range(1, segs + 1):
        f = i / segs
        base = (a[0] + dx * f, a[1] + dy * f)
        off = jitter(seed + i) * 2.2  # draw_logo uses sketchy_line, i.e. t = 0
        points.append(b if i == segs else (base[0] + px * off, base[1] + py * off))
    c.lines(points, color, width)


def sketchy_poly(c, pts, color, width, seed):
    for a, b in zip(pts, pts[1:]):
        sketchy_line(c, a, b, color, width, seed)


def sketchy_circle(c, center, radius, color, width, seed):
    n = 26
    points = [(center[0] + radius, center[1])]
    for i in range(1, n + 1):
        ang = i / n * math.tau
        wob = 1.0 + jitter(seed + i) * 0.04
        points.append(
            (
                center[0] + math.cos(ang) * radius * wob,
                center[1] + math.sin(ang) * radius * wob,
            )
        )
    c.lines(points, color, width)


def draw_logo(c, t):
    # back photo peeking out top-right, tilted the other way
    back = [(28, 10), (74, 16), (70, 54), (24, 48)]
    c.poly(back, BACK)
    sketchy_poly(c, back, INK_SOFT, 3.0, 11)
    sketchy_line(c, back[3], back[0], INK_SOFT, 3.0, 507)

    # front photo — the main card
    front = [(12, 26), (72, 22), (76, 78), (16, 82)]
    c.poly(front, FRONT)
    sketchy_poly(c, front, INK, 3.5, 77)
    sketchy_line(c, front[3], front[0], INK, 3.5, 877)

    # mountains inside the photo
    sketchy_poly(c, [(20, 74), (38, 48), (48, 62), (60, 42), (70, 72)], BLUE, 3.0, 33)

    # little pine on the slope
    sketchy_poly(c, [(30, 76), (36, 62), (42, 76), (30, 76)], GREEN, 2.5, 55)

    # sun inside the photo, top-right corner
    sketchy_circle(c, (58, 38), 6.0, RED, 3.0, 99)
    for i in range(8):
        a = i / 8.0 * math.tau + t * 0.35
        sketchy_line(
            c,
            (58.0 + math.cos(a) * 9.0, 38.0 + math.sin(a) * 9.0),
            (58.0 + math.cos(a) * 12.0, 38.0 + math.sin(a) * 12.0),
            RED,
            2.5,
            200 + i,
        )

    # drifting, bobbing cloud above the photo — clear of the sun
    bob = math.sin(t * 2.2) * 3.5
    drift = math.sin(t * 0.9) * 5.0
    cloud = [
        (22.0 + drift, 16.0 + bob),
        (18.0 + drift, 11.0 + bob),
        (25.0 + drift, 5.0 + bob),
        (33.0 + drift, 9.0 + bob),
        (40.0 + drift, 3.0 + bob),
        (46.0 + drift, 9.0 + bob),
        (52.0 + drift, 6.0 + bob),
        (54.0 + drift, 12.0 + bob),
        (47.0 + drift, 15.0 + bob),
        (39.0 + drift, 16.0 + bob),
        (30.0 + drift, 17.0 + bob),
    ]
    c.poly(cloud, WHITE)
    sketchy_poly(c, cloud, INK, 3.0, 404)
    sketchy_line(c, cloud[10], cloud[0], INK, 3.0, 540)


def frame(t, scale=12, supersample=2):
    c = Canvas(scale * supersample)
    draw_logo(c, t)
    return c.img.resize((W * scale, H * scale), Image.LANCZOS)


def main():
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0
    fps = int(sys.argv[3]) if len(sys.argv) > 3 else 20
    out.mkdir(parents=True, exist_ok=True)
    frame(0.0).save(out / "logo-native.png")
    count = round(seconds * fps)
    for n in range(count):
        frame(n / fps).save(out / f"f{n:04d}.png")
    print(f"wrote {count} frames + logo-native.png to {out}")


if __name__ == "__main__":
    main()
