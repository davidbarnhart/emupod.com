#!/usr/bin/env python3
"""The offline app's home-screen icon: the click wheel (emupod/offline/icon-wheel.jpg, David's picture of it), cropped square and
reduced to emupod/offline/icon-180.png (iOS), icon-192.png and icon-512.png (the manifest). Needs Pillow; the PNGs are committed,
so the build does not."""
import os
from PIL import Image

D = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "emupod/offline")
im = Image.open(os.path.join(D, "icon-wheel.jpg")).convert("RGB")
w, h = im.size; s = min(w, h)
im = im.crop(((w - s) // 2, (h - s) // 2, (w - s) // 2 + s, (h - s) // 2 + s))
for n in (180, 192, 512):
    im.resize((n, n), Image.LANCZOS).save(os.path.join(D, "icon-%d.png" % n), optimize=True); print("icon-%d.png" % n)
