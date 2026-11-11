#!/usr/bin/env python3
"""Compute the aarch64 runtime closure of GTK4/libadwaita/gtk4-layer-shell."""
import os
import subprocess
import sys

ROOT = "/home/ivan/phosh-state/gtk4-root"
STARTS = [
    "libgtk-4.so.1",
    "libadwaita-1.so.0",
    "libgtk4-layer-shell.so.0",
]


def index_libraries():
    index = {}
    for base in (f"{ROOT}/usr/lib", f"{ROOT}/lib"):
        if not os.path.isdir(base):
            continue
        for entry in os.listdir(base):
            if ".so" not in entry:
                continue
            path = os.path.join(base, entry)
            if os.path.islink(path):
                target = os.path.realpath(path)
                if os.path.exists(target):
                    index.setdefault(entry, target)
            elif os.path.isfile(path):
                index.setdefault(entry, path)
    return index


def needed(path):
    try:
        output = subprocess.check_output(
            ["aarch64-linux-gnu-objdump", "-p", path],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return []
    result = []
    for line in output.splitlines():
        if "NEEDED" in line:
            result.append(line.split()[-1])
    return result


def main():
    index = index_libraries()
    seen = {}
    queue = list(STARTS)
    while queue:
        name = queue.pop()
        if name in seen:
            continue
        path = index.get(name)
        if path is None:
            print(f"MISSING {name}", file=sys.stderr)
            continue
        seen[name] = path
        for child in needed(path):
            if child not in seen:
                queue.append(child)
    total = 0
    for name in sorted(seen):
        size = os.path.getsize(seen[name])
        total += size
        print(f"{size:10d} {name} -> {seen[name]}")
    print(f"# {len(seen)} libraries, {total} bytes ({total/1048576:.1f} MiB)")


if __name__ == "__main__":
    main()
