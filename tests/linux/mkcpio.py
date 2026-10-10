#!/usr/bin/env python3
"""Write a newc cpio archive (initramfs) from (path, mode, data) entries.

usage: mkcpio.py OUT DEST=SRC [DEST=SRC ...] [DIR/ ...]   (files are installed 0755)
Directories in DEST paths are created automatically, plus /dev/console.
"""
import os
import sys


def entry(name, mode, data=b"", ino=[1], rdev=(0, 0)):
    ino[0] += 1
    name_b = name.encode() + b"\0"
    hdr = "070701" + "".join(
        "%08x" % v
        for v in (ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(name_b), 0)
    )
    out = hdr.encode() + name_b
    out += b"\0" * (-len(out) % 4)
    out += data + b"\0" * (-len(data) % 4)
    return out


def main():
    out_path, specs = sys.argv[1], sys.argv[2:]
    blob = b""
    dirs = {"dev"}
    files = []
    for spec in specs:
        if spec.endswith("/"):
            dirs.add(spec.strip("/"))
            continue
        dest, src = spec.split("=", 1)
        dest = dest.strip("/")
        parts = dest.split("/")[:-1]
        for i in range(1, len(parts) + 1):
            dirs.add("/".join(parts[:i]))
        with open(src, "rb") as f:
            files.append((dest, f.read()))
    for d in sorted(dirs):
        blob += entry(d, 0o040755)
    blob += entry("dev/console", 0o020600, rdev=(5, 1))
    for dest, data in files:
        blob += entry(dest, 0o100755, data)
    blob += entry("TRAILER!!!", 0)
    blob += b"\0" * (-len(blob) % 512)
    with open(out_path, "wb") as f:
        f.write(blob)


if __name__ == "__main__":
    main()
