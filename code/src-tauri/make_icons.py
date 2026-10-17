"""Generate the application icon set.

Run once, from the repo, then delete: the generated images are the artifact.
Outputs the exact files listed in `tauri.conf.json` -> `bundle.icon`.
"""

from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent / "icons"
OUT.mkdir(parents=True, exist_ok=True)

SIZE = 1024
SS = 4  # supersample factor for smooth edges
C = SIZE * SS

BRAND_TOP = (254, 44, 85)
BRAND_BOTTOM = (198, 22, 66)


def rounded_mask(size: int, radius: int) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)
    return mask


def gradient(size: int, top, bottom) -> Image.Image:
    img = Image.new("RGB", (size, size))
    draw = ImageDraw.Draw(img)
    for y in range(size):
        t = y / (size - 1)
        colour = tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3))
        draw.line((0, y, size, y), fill=colour)
    return img


def build_icon() -> Image.Image:
    base = gradient(C, BRAND_TOP, BRAND_BOTTOM).convert("RGBA")

    glyph = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    draw = ImageDraw.Draw(glyph)

    def px(v: float) -> float:
        return v * C / SIZE

    white = (255, 255, 255, 255)

    # Download arrow: shaft + head.
    draw.rounded_rectangle(
        (px(462), px(250), px(562), px(556)), radius=px(28), fill=white
    )
    draw.polygon(
        [(px(352), px(516)), (px(672), px(516)), (px(512), px(716))], fill=white
    )

    # Tray line under the arrow.
    draw.rounded_rectangle(
        (px(288), px(800), px(736), px(862)), radius=px(31), fill=white
    )

    base.alpha_composite(glyph)
    base.putalpha(rounded_mask(C, radius=round(px(224))))
    return base.resize((SIZE, SIZE), Image.LANCZOS)


def write_icns(pngs: dict[str, bytes], path: Path) -> None:
    """Write a minimal ICNS container holding PNG-encoded entries."""
    body = b""
    for icns_type, data in pngs.items():
        body += icns_type.encode("ascii") + (len(data) + 8).to_bytes(4, "big") + data
    path.write_bytes(b"icns" + (len(body) + 8).to_bytes(4, "big") + body)


def main() -> None:
    icon = build_icon()

    icon.resize((32, 32), Image.LANCZOS).save(OUT / "32x32.png")
    icon.resize((128, 128), Image.LANCZOS).save(OUT / "128x128.png")
    icon.resize((256, 256), Image.LANCZOS).save(OUT / "128x128@2x.png")

    icon.resize((256, 256), Image.LANCZOS).save(
        OUT / "icon.ico",
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    import io

    def png_bytes(size: int) -> bytes:
        buffer = io.BytesIO()
        icon.resize((size, size), Image.LANCZOS).save(buffer, format="PNG")
        return buffer.getvalue()

    write_icns(
        {
            "ic07": png_bytes(128),  # 128x128
            "ic08": png_bytes(256),  # 256x256
            "ic09": png_bytes(512),  # 512x512
        },
        OUT / "icon.icns",
    )

    for produced in sorted(OUT.iterdir()):
        print(f"{produced.name}: {produced.stat().st_size} bytes")


if __name__ == "__main__":
    main()
