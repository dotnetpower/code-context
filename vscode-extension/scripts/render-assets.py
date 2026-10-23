"""Optional developer tool; rendered PNGs are committed and used by VSCE."""

from pathlib import Path

import cairosvg

resources = Path(__file__).resolve().parents[1] / "resources"
for name in ("icon", "banner"):
    cairosvg.svg2png(
        url=str(resources / f"{name}.svg"),
        write_to=str(resources / f"{name}.png"),
    )
