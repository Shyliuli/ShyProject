#!/usr/bin/env python3
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) not in (3, 4):
        print(
            "usage: embed_user_image.py <input.sfs> <output.shy> [symbol_prefix]",
            file=sys.stderr,
        )
        return 2

    data = Path(sys.argv[1]).read_bytes()
    out = Path(sys.argv[2])
    prefix = sys.argv[3] if len(sys.argv) == 4 else "user"
    image = f"{prefix}_image"
    image_end = f"{prefix}_image_end"
    lines = [
        "___DEFINE___",
        "",
        "___DATA___",
        "",
        f".section data.{image}",
        f".symbol {image}",
    ]

    chunk = 24
    for offset in range(0, len(data), chunk):
        items = ",".join(str(b) for b in data[offset : offset + chunk])
        lines.append(f"{image}({offset}) [{items}]")

    lines.extend(
        [
            f".symbol {image_end}",
            "",
            "___CODE___",
            "",
            f".section text.{image}_dummy",
        ]
    )
    out.write_text("\n".join(lines), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
