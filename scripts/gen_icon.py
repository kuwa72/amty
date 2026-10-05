#!/usr/bin/env python3
"""Generate amty app icon (chat bubble + robot face) as PNG."""
from PIL import Image, ImageDraw, ImageFont
import math

SIZE = 512
PADDING = 40
BG = (0, 0, 0, 0)
BUBBLE = (74, 144, 217, 255)   # #4A90D9
ROBOT = (255, 255, 255, 255)
DARK = (30, 30, 30, 255)

img = Image.new("RGBA", (SIZE, SIZE), BG)
d = ImageDraw.Draw(img)

# Chat bubble: rounded rect + tail
x0, y0 = PADDING, PADDING
x1, y1 = SIZE - PADDING, SIZE - PADDING * 2
r = 60
d.rounded_rectangle([x0, y0, x1, y1], radius=r, fill=BUBBLE)
# tail at bottom-center
tail_x = x0 + (x1 - x0) // 2
d.polygon(
    [(tail_x - 20, y1 - 5), (tail_x + 20, y1 - 5), (tail_x, y1 + 30)],
    fill=BUBBLE,
)

# Robot head inside bubble (centered)
cx = SIZE // 2
cy = (y0 + y1) // 2
head_w, head_h = 200, 140
hw, hh = head_w // 2, head_h // 2

# head shape (rounded rect, slightly wider at top)
d.rounded_rectangle(
    [cx - hw, cy - hh, cx + hw, cy + hh],
    radius=35,
    fill=ROBOT,
)

# eyes (two circles, slightly up from center)
eye_y = cy - 20
eye_r = 18
for ex in (cx - 55, cx + 55):
    d.ellipse([ex - eye_r, eye_y - eye_r, ex + eye_r, eye_y + eye_r], fill=DARK)
    # highlight
    hl = eye_r // 3
    d.ellipse([ex - 5, eye_y - 5, ex - 5 + hl, eye_y - 5 + hl], fill=(255, 255, 255, 200))

# antenna stem + ball on top
stem_w = 10
stem_h = 35
ball_r = 16
stem_top = cy - hh - stem_h
stem_bot = cy - hh + 2
d.rectangle(
    [cx - stem_w // 2, stem_top, cx + stem_w // 2, stem_bot],
    fill=ROBOT,
)
d.ellipse(
    [cx - ball_r, stem_top - ball_r, cx + ball_r, stem_top + ball_r],
    fill=ROBOT,
)

# mouth (small rounded rect at bottom of face)
mouth_w, mouth_h = 60, 10
d.rounded_rectangle(
    [cx - mouth_w // 2, cy + 30, cx + mouth_w // 2, cy + 30 + mouth_h],
    radius=5,
    fill=DARK,
)

img.save("assets/amty-icon.png")
img.save(
    "assets/amty-icon.ico",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print("wrote assets/amty-icon.png / assets/amty-icon.ico")
