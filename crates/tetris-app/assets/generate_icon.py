#!/usr/bin/env python3
"""Generate icon.png — 1024x1024 app icon for Blockfall (plan T21).

Flat-color tetromino composition using the in-game palette from
crates/tetris-app/src/render.rs (PIECE_RGB, Tetris Guideline colors).
Deterministic: pure geometry, no randomness.

Usage: python3 generate_icon.py   (writes icon.png next to this file)
"""

from PIL import Image, ImageDraw

SIZE = 1024
BG = (20, 25, 38, 255)          # deep navy backdrop
CORNER = 184                    # app-icon squircle radius
CELL = 168                      # tetromino cell size
PITCH = 188                     # cell stride (gap = 20)
CELL_RADIUS = 26
X0 = 146                        # grid origin (4 cols centered)
Y0 = 208

# Piece palette (sRGB 0-255), matching render.rs PIECE_RGB:
I, J, L, O, S, T, Z = (
    (0, 191, 191),
    (64, 89, 217),
    (230, 128, 26),
    (242, 204, 26),
    (51, 179, 64),
    (166, 77, 204),
    (217, 51, 51),
)

# (col, row, rgb) — floating T above a settled mixed stack.
CELLS = [
    (1, 0, T), (2, 0, T), (3, 0, T), (2, 1, T),
    (0, 1, I), (0, 2, I),
    (3, 1, O), (3, 2, O),
    (0, 3, J), (1, 3, L), (2, 3, S), (3, 3, Z),
]


def main() -> None:
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle((0, 0, SIZE - 1, SIZE - 1), radius=CORNER, fill=BG)

    for col, row, rgb in CELLS:
        x = X0 + col * PITCH
        y = Y0 + row * PITCH
        draw.rounded_rectangle(
            (x, y, x + CELL - 1, y + CELL - 1),
            radius=CELL_RADIUS,
            fill=(*rgb, 255),
        )

    img.save("icon.png")
    print(f"wrote icon.png ({SIZE}x{SIZE})")


if __name__ == "__main__":
    main()
