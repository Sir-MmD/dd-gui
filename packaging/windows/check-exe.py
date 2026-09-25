#!/usr/bin/env python3
"""Checks the resources build.rs puts into dd-gui.exe.

    python3 packaging/windows/check-exe.py path/to/dd-gui.exe

Passes when the exe has:
  - the icon: one icon group holding every image of assets/dd-gui.ico, byte for byte;
  - the version info: ProductName and FileDescription "DD-GUI", InternalName "dd-gui",
    OriginalFilename "dd-gui.exe", and the version from Cargo.toml (strings and numbers);
  - exactly one manifest, for an assembly named "dd-gui", asking for administrator rights.

Standard library only (reads the PE resource tree itself), so it runs on any CI runner.
"""
import argparse
import os
import re
import struct
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

RT_ICON, RT_GROUP_ICON, RT_VERSION, RT_MANIFEST = 3, 14, 16, 24


def cargo_version():
    text = open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8").read()
    package = text.split("[package]", 1)[1].split("\n[", 1)[0]
    return re.search(r'^version\s*=\s*"([^"]+)"', package, re.M).group(1)


def resources(exe):
    """{(type, name, lang): bytes} from the PE's resource directory."""
    data = open(exe, "rb").read()
    if data[:2] != b"MZ":
        sys.exit(f"{exe}: not an exe")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        sys.exit(f"{exe}: not a PE file")
    sections, opt_size = struct.unpack_from("<H12xH", data, pe + 6)
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    dirs = opt + {0x10B: 96, 0x20B: 112}[magic]
    rsrc_rva, rsrc_size = struct.unpack_from("<II", data, dirs + 2 * 8)
    if not rsrc_rva or not rsrc_size:
        print(f"{exe}: no resources at all", file=sys.stderr)
        return {}
    table = []
    for i in range(sections):
        _name, vsize, va, raw_size, raw = struct.unpack_from("<8sIIII", data, opt + opt_size + 40 * i)
        table.append((va, max(vsize, raw_size), raw))

    def offset(rva):
        for va, size, raw in table:
            if va <= rva < va + size:
                return raw + rva - va
        raise ValueError(f"RVA {rva:#x} is in no section")

    try:
        base = offset(rsrc_rva)
    except ValueError:
        print(f"{exe}: the resource directory points outside every section", file=sys.stderr)
        return {}

    def entries(dir_off):
        named, ids = struct.unpack_from("<12xHH", data, base + dir_off)
        for i in range(named + ids):
            name, target = struct.unpack_from("<II", data, base + dir_off + 16 + 8 * i)
            if name & 0x80000000:
                n = base + (name & 0x7FFFFFFF)
                length = struct.unpack_from("<H", data, n)[0]
                name = data[n + 2:n + 2 + 2 * length].decode("utf-16-le")
            yield name, target

    found = {}
    for rtype, t in entries(0):
        for name, n in entries(t & 0x7FFFFFFF):
            for lang, leaf in entries(n & 0x7FFFFFFF):
                rva, size = struct.unpack_from("<II", data, base + leaf)
                start = offset(rva)
                found[(rtype, name, lang)] = data[start:start + size]
    return found


def version_strings(blob):
    """(fixed file version, product version, {key: value}) from a VS_VERSIONINFO."""
    def align(p):
        return (p + 3) & ~3

    def utf16z(p, end):
        q = p
        while q + 1 < end and blob[q:q + 2] != b"\0\0":
            q += 2
        return blob[p:q].decode("utf-16-le"), q + 2

    def node(p):
        length, value_len, text = struct.unpack_from("<HHH", blob, p)
        end = p + length
        key, q = utf16z(p + 6, end)
        q = align(q)
        if text:
            value, after = utf16z(q, end) if value_len else ("", q)
        else:
            value, after = blob[q:q + value_len], q + value_len
        children, q = [], align(after)
        while q + 6 <= end:
            child, child_end = node(q)
            if child_end <= q:
                break
            children.append(child)
            q = align(child_end)
        return (key, value, children), end

    (key, fixed, children), _ = node(0)
    assert key == "VS_VERSION_INFO", key
    sig, _, fms, fls, pms, pls = struct.unpack_from("<6I", fixed)
    assert sig == 0xFEEF04BD, hex(sig)
    words = lambda ms, ls: (ms >> 16, ms & 0xFFFF, ls >> 16, ls & 0xFFFF)
    strings = {}
    for k, _, tables in children:
        if k == "StringFileInfo":
            for _, _, pairs in tables:
                for name, value, _ in pairs:
                    strings[name] = value
    return words(fms, fls), words(pms, pls), strings


def ico_images(path):
    data = open(path, "rb").read()
    _, kind, count = struct.unpack_from("<HHH", data, 0)
    assert kind == 1, "not an .ico"
    images = {}
    for i in range(count):
        w, h, _, _, _, _, size, off = struct.unpack_from("<BBBBHHII", data, 6 + 16 * i)
        images[w or 256] = data[off:off + size]
    return images


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("exe")
    ap.add_argument("--version", help="expected version (default: Cargo.toml's)")
    ap.add_argument("--ico", default=os.path.join(ROOT, "assets", "dd-gui.ico"))
    args = ap.parse_args()
    version = args.version or cargo_version()
    res = resources(args.exe)
    problems = []

    # Icon
    groups = sorted((k for k in res if k[0] == RT_GROUP_ICON), key=str)
    icons = {k[1]: v for k, v in res.items() if k[0] == RT_ICON}
    expected = ico_images(args.ico)
    if len(groups) != 1:
        problems.append(f"{len(groups)} icon groups, expected 1")
    for key in groups[:1]:
        grp = res[key]
        _, kind, count = struct.unpack_from("<HHH", grp, 0)
        sizes = []
        for i in range(count):
            w, h, _, _, _, bits, size, icon_id = struct.unpack_from("<BBBBHHIH", grp, 6 + 14 * i)
            w = w or 256
            sizes.append(w)
            image = icons.get(icon_id)
            if image is None:
                problems.append(f"icon {w}px: RT_ICON {icon_id} is missing")
            elif image != expected.get(w):
                problems.append(f"icon {w}px differs from {os.path.basename(args.ico)}")
        print(f"icon group {key[1]}: {', '.join(f'{s}px' for s in sizes)}")
        if sorted(sizes) != sorted(expected):
            problems.append(f"icon sizes {sorted(sizes)}, expected {sorted(expected)}")

    # Version info
    versions = [k for k in res if k[0] == RT_VERSION]
    if len(versions) != 1:
        problems.append(f"{len(versions)} version resources, expected 1")
    else:
        file_ver, product_ver, strings = version_strings(res[versions[0]])
        for k in ("ProductName", "FileDescription", "InternalName", "OriginalFilename", "FileVersion", "ProductVersion"):
            print(f"{k}: {strings.get(k)!r}")
        numbers = tuple(int(x) for x in re.findall(r"\d+", version)[:3]) + (0,)
        want = {"ProductName": "DD-GUI", "FileDescription": "DD-GUI", "InternalName": "dd-gui",
                "OriginalFilename": "dd-gui.exe", "FileVersion": version, "ProductVersion": version}
        for k, v in want.items():
            if strings.get(k) != v:
                problems.append(f"{k} is {strings.get(k)!r}, expected {v!r}")
        if file_ver != numbers or product_ver != numbers:
            problems.append(f"version numbers {file_ver} / {product_ver}, expected {numbers}")

    # Manifest
    manifests = [k for k in res if k[0] == RT_MANIFEST]
    print(f"manifests: {len(manifests)}")
    if len(manifests) != 1:
        problems.append(f"{len(manifests)} manifests, expected exactly 1")
    for key in manifests:
        xml = res[key].decode("utf-8", "replace")
        level = re.search(r'requestedExecutionLevel[^>]*level="([^"]+)"', xml)
        name = re.search(r'<assemblyIdentity[^>]*name="([^"]+)"', xml)
        print(f"manifest {key[1]}/{key[2]}: level={level and level.group(1)}, name={name and name.group(1)}")
        if not level or level.group(1) != "requireAdministrator":
            problems.append("the manifest doesn't ask for administrator rights")
        if not name or name.group(1) != "dd-gui":
            problems.append(f"the manifest's assembly is {name and name.group(1)!r}, expected 'dd-gui'")

    for p in problems:
        print(f"FAIL: {p}", file=sys.stderr)
    if problems:
        return 1
    print(f"{args.exe}: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
