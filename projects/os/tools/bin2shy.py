#!/usr/bin/env python3
import argparse
import os
import re
from pathlib import Path


C_KEYWORDS = {
    "auto", "break", "case", "char", "const", "continue", "default", "do",
    "double", "else", "enum", "extern", "float", "for", "goto", "if",
    "inline", "int", "long", "register", "restrict", "return", "short",
    "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
    "unsigned", "void", "volatile", "while",
}


def c_identifier(path, used):
    stem = Path(path).stem
    ident = re.sub(r"[^0-9A-Za-z_]", "_", stem)
    ident = re.sub(r"_+", "_", ident).strip("_")
    if not ident:
        ident = "file"
    if ident[0].isdigit() or ident in C_KEYWORDS:
        ident = "fs_" + ident

    base = ident
    suffix = 2
    while ident in used:
        ident = f"{base}_{suffix}"
        suffix += 1
    used.add(ident)
    return ident


def byte_initializer(data):
    if not data:
        return "0"

    lines = []
    for i in range(0, len(data), 12):
        chunk = data[i:i + 12]
        lines.append("    " + ", ".join(str(b) for b in chunk))
    return ",\n".join(lines)


def write_c(output, inputs):
    used = set()
    files = []
    data_ids = {}
    for path in inputs:
        with open(path, "rb") as f:
            data = f.read()
        embedded_name = Path(path).stem if Path(path).suffix == ".sfs" else Path(path).name
        ident = c_identifier(path, used)
        files.append({
            "name": embedded_name,
            "id": ident,
            "data": data,
            # 内容相同的文件（例如 busybox 的各个 applet）共用一个 data 数组。
            "data_id": data_ids.setdefault(data, ident),
        })

    with open(output, "w", encoding="utf-8") as out:
        out.write("#![no_main]\n\n")
        out.write("struct fs_file {\n")
        out.write("    char *name;\n")
        out.write("    unsigned char *data;\n")
        out.write("    unsigned int size;\n")
        out.write("};\n\n")

        for item in files:
            ident = item["id"]
            name_bytes = item["name"].encode("utf-8") + b"\0"
            out.write(f"unsigned char {ident}_name[] = {{\n")
            out.write(byte_initializer(name_bytes))
            out.write("\n};\n\n")

        for item in files:
            if item["data_id"] != item["id"]:
                continue
            ident = item["id"]
            data = item["data"]
            out.write(f"unsigned char {ident}_data[] = {{\n")
            out.write(byte_initializer(data))
            out.write("\n};\n\n")
            out.write(f"unsigned int {ident}_len = {len(data)};\n\n")

        if files:
            out.write(f"struct fs_file fs_files[{len(files)}] = {{\n")
            for item in files:
                out.write(
                    f"    {{ (char *){item['id']}_name, {item['data_id']}_data, {len(item['data'])} }},\n"
                )
            out.write("};\n")
        else:
            out.write("struct fs_file fs_files[1] = {\n")
            out.write("    { (char *)0, (unsigned char *)0, 0 },\n")
            out.write("};\n")

        out.write(f"int fs_nfiles = {len(files)};\n\n")
        out.write("int fs_init(void) {\n")
        out.write("    return 0;\n")
        out.write("}\n")


def main():
    parser = argparse.ArgumentParser(
        description="Convert .sfs files into ShyC byte arrays."
    )
    parser.add_argument("-o", "--output", required=True)
    parser.add_argument("inputs", nargs="*")
    args = parser.parse_args()

    os.makedirs(os.path.dirname(args.output) or ".", exist_ok=True)
    write_c(args.output, args.inputs)


if __name__ == "__main__":
    main()
