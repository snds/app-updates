#!/usr/bin/env python3
"""Stamps a main-channel version into an app's manifests, before its build.

    python3 release/stamp-version.py <version> <tauri dir> [repo root]

The version is the app's own with a pre-release added:
`0.1.0-main.20261107093000` (the commit time, UTC). The app's own version is
the one in `<tauri dir>/tauri.conf.json`, which the updater compares. Any
other base is refused, and so is a second stamp.

It is written to:

- `<tauri dir>/tauri.conf.json` (the app's version and the bundles' names);
- the app crate's version: `<tauri dir>/Cargo.toml`, or, when that says
  `version.workspace = true`, the workspace's `[workspace.package]` version,
  with every path dependency's version requirement on it;
- the matching entries in `Cargo.lock`, so a `--locked` build still agrees;
- `package.json` beside the tauri dir, when it carries the same version.

Only the version lines change, so each file keeps its layout.
Ported from Lockdown's packaging/update/stamp-version.py.
"""

import json
import re
import sys
from pathlib import Path

VERSION = re.compile(r"^(\d+\.\d+\.\d+)-main\.(\d{14})$")
SKIP = ("node_modules", "target")


def _package_section(text, header):
    return re.search(r"(?ms)^\[" + re.escape(header) + r"\]\n(.*?)(?=^\[|\Z)", text)


def _replace_in_section(path, header, base, version):
    text = path.read_text()
    section = _package_section(text, header)
    line = re.compile(r'(?m)^version\s*=\s*"' + re.escape(base) + r'"')
    if not section or not line.search(section.group(1)):
        return False
    start, end = section.span(1)
    body = line.sub(f'version = "{version}"', section.group(1), count=1)
    path.write_text(text[:start] + body + text[end:])
    return True


def _crate_name(text):
    section = _package_section(text, "package")
    m = section and re.search(r'(?m)^name\s*=\s*"([^"]+)"', section.group(1))
    if not m:
        raise SystemExit("stamp-version: the app's Cargo.toml has no [package] name")
    return m.group(1)


def _names(line, crates):
    """Whether a dependency line is on one of `crates` (by key or `package =`)."""
    return any(re.match(r'\s*"?' + re.escape(c) + r'"?\s*=', line) or f'package = "{c}"' in line for c in crates)


def _upward(start, root, name, want=None):
    """The nearest `name` from `start` up to `root`, optionally containing `want`."""
    here = start
    while True:
        candidate = here / name
        if candidate.is_file() and (want is None or want in candidate.read_text()):
            return candidate
        if here == root or here.parent == here:
            return None
        here = here.parent


def stamp(root: Path, tauri_dir: str, version: str) -> list[str]:
    m = VERSION.match(version)
    if not m:
        raise SystemExit(f"stamp-version: {version!r} isn't <x.y.z>-main.<YYYYMMDDHHMMSS>")
    base = m.group(1)
    root = root.resolve()
    app = (root / tauri_dir).resolve()
    changed = []

    conf = app / "tauri.conf.json"
    if not conf.is_file():
        raise SystemExit(f"stamp-version: no tauri.conf.json in {tauri_dir}")
    have = json.loads(conf.read_text()).get("version")
    if have != base:
        raise SystemExit(f"stamp-version: {tauri_dir}/tauri.conf.json says {have!r}, not {base}")
    text = conf.read_text().replace(f'"version": "{base}"', f'"version": "{version}"', 1)
    if json.loads(text).get("version") != version:
        raise SystemExit(f"stamp-version: couldn't write the version into {tauri_dir}/tauri.conf.json")
    conf.write_text(text)
    changed.append(conf.relative_to(root).as_posix())

    manifest = app / "Cargo.toml"
    if not manifest.is_file():
        raise SystemExit(f"stamp-version: no Cargo.toml in {tauri_dir}")
    crate_text = manifest.read_text()
    crates = {_crate_name(crate_text)}
    section = _package_section(crate_text, "package").group(1)
    if re.search(r"(?m)^version\s*\.\s*workspace\s*=\s*true|^version\s*=\s*\{\s*workspace\s*=\s*true", section):
        workspace = _upward(app.parent, root, "Cargo.toml", "[workspace.package]")
        if not workspace or not _replace_in_section(workspace, "workspace.package", base, version):
            raise SystemExit(f"stamp-version: the workspace's version isn't {base}")
        changed.append(workspace.relative_to(root).as_posix())
        # Every crate that inherits the version moves with it.
        for other in sorted(workspace.parent.rglob("Cargo.toml")):
            if any(p in SKIP or p.startswith(".") for p in other.relative_to(workspace.parent).parts) or other == workspace:
                continue
            t = other.read_text()
            s = _package_section(t, "package")
            if s and re.search(r"(?m)^version\s*(\.\s*workspace|=\s*\{\s*workspace)", s.group(1)):
                crates.add(_crate_name(t))
    else:
        if not _replace_in_section(manifest, "package", base, version):
            raise SystemExit(f"stamp-version: {tauri_dir}/Cargo.toml's version isn't {base}")
        changed.append(manifest.relative_to(root).as_posix())

    # A path dependency's requirement on a stamped crate ("0.1.0") doesn't
    # match a pre-release, so it names the stamped version too.
    path_dep = re.compile(r'^(.*\bpath\s*=\s*"[^"]*".*\bversion\s*=\s*)"' + re.escape(base) + r'"', re.M)
    for other in sorted(root.rglob("Cargo.toml")):
        if any(p in SKIP or p.startswith(".") for p in other.relative_to(root).parts):
            continue
        before = other.read_text()
        after, n = path_dep.subn(
            lambda d: f'{d.group(1)}"{version}"' if _names(d.group(0), crates) else d.group(0),
            before,
        )
        if after != before:
            other.write_text(after)
            changed.append(f"{other.relative_to(root).as_posix()} (path dependencies)")

    lock = _upward(app, root, "Cargo.lock")
    if lock:
        blocks = lock.read_text().split("[[package]]")
        count = 0
        for i, block in enumerate(blocks[1:], start=1):
            name = re.search(r'\nname = "([^"]+)"\n', block)
            if "source = " in block or not name or name.group(1) not in crates:
                continue
            if f'\nversion = "{base}"\n' in block:
                blocks[i] = block.replace(f'\nversion = "{base}"\n', f'\nversion = "{version}"\n', 1)
                count += 1
        if count == 0:
            raise SystemExit(f"stamp-version: {lock.relative_to(root)} has no {sorted(crates)} at {base}")
        lock.write_text("[[package]]".join(blocks))
        changed.append(f"{lock.relative_to(root).as_posix()} ({count} crates)")

    package = app.parent / "package.json"
    if package.is_file() and json.loads(package.read_text()).get("version") == base:
        text = package.read_text().replace(f'"version": "{base}"', f'"version": "{version}"', 1)
        package.write_text(text)
        changed.append(package.relative_to(root).as_posix())
    return changed


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        raise SystemExit(__doc__)
    root = Path(sys.argv[3] if len(sys.argv) == 4 else ".")
    for name in stamp(root, sys.argv[2], sys.argv[1]):
        print(f"stamped {sys.argv[1]} into {name}")
