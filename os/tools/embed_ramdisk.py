#!/usr/bin/env python3
import sys
from pathlib import Path


def symbol_for(name: str) -> str:
    if name == ".":
        return "rd_dot"
    out = []
    for ch in name:
        if ch.isalnum():
            out.append(ch.lower())
        else:
            out.append("_")
    return "rd_" + "".join(out)


def emit_bytes(lines: list[str], symbol: str, data: bytes) -> None:
    lines.append(f".section data.{symbol}")
    lines.append(f".symbol {symbol}")
    chunk = 24
    if not data:
        lines.append(f"{symbol}(0) []")
    for offset in range(0, len(data), chunk):
        items = ",".join(str(b) for b in data[offset : offset + chunk])
        lines.append(f"{symbol}({offset}) [{items}]")
    lines.append(f".symbol {symbol}_end")
    lines.append("")


def main() -> int:
    if len(sys.argv) < 4 or (len(sys.argv) - 2) % 2 != 0:
        print(
            "usage: embed_ramdisk.py <output.shy> <name> <path> [<name> <path> ...]",
            file=sys.stderr,
        )
        return 2

    out = Path(sys.argv[1])
    entries: list[tuple[str, bytes]] = []
    for i in range(2, len(sys.argv), 2):
        name = sys.argv[i]
        path = Path(sys.argv[i + 1])
        entries.append((name, path.read_bytes()))

    listing = "".join(name + "\n" for name, _ in entries).encode("ascii")
    entries.append((".", listing))

    lines = ["___DEFINE___", "", "___DATA___", ""]
    for name, data in entries:
        sym = symbol_for(name)
        emit_bytes(lines, sym + "_name", name.encode("ascii") + b"\0")
        emit_bytes(lines, sym + "_data", data)

    lines.extend(["___CODE___", "", ".section text.ramdisk_dummy"])
    out.write_text("\n".join(lines), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
