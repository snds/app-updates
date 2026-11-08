#!/usr/bin/env python3
"""Writes an app's updater manifest `latest.json` for its main channel, in
the static form Tauri's updater reads, and refuses to take the channel back.

    python3 release/write-manifest.py --app fontface \\
        --version 0.1.0-main.20261107093000 --pub-date 2026-11-07T09:30:00Z \\
        --bundle darwin=dist/Fontface_..._universal.app.tar.gz \\
        --bundle windows=dist/Fontface_..._x64-setup.exe \\
        [--bundle linux=dist/fontface_..._amd64.deb] \\
        [--repo snds/releases] [--previous published.json] --out dist/latest.json

The files are published on the app's rolling pre-release `<app>-main` in
the releases repo, so every URL is
`https://github.com/<repo>/releases/download/<app>-main/<file>`.

Each bundle has its minisign signature beside it (`<file>.sig`, as
`tauri build` and `tauri signer sign` write it). Refused, with a reason:

- an app id that isn't lower-case letters, digits and dashes;
- a version that isn't `<x.y.z>-main.<YYYYMMDDHHMMSS>`;
- a missing or empty signature, or one that isn't a minisign signature
  bound to this version (its trusted comment names `version:<version>`,
  which the app requires with `requireSignedVersion`);
- with --previous, a version that isn't newer than the one published, so
  a late run never takes the channel backwards.

The app reads the platform keys `darwin-aarch64`, `darwin-x86_64` (one
universal bundle), `windows-x86_64` (the NSIS installer) and
`linux-x86_64` (the deb, which the app only links to).
Ported from Lockdown's packaging/update/write-manifest.py.
"""

import argparse
import base64
import json
import re
import sys
from pathlib import Path
from urllib.parse import quote

VERSION = re.compile(r"^(\d+)\.(\d+)\.(\d+)-main\.(\d{14})$")
APP = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
REPO = re.compile(r"^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$")
DEFAULT_REPO = "snds/releases"

PLATFORMS = {
    "darwin": ["darwin-aarch64", "darwin-x86_64", "darwin-aarch64-app", "darwin-x86_64-app"],
    "windows": ["windows-x86_64", "windows-x86_64-nsis"],
    "linux": ["linux-x86_64", "linux-x86_64-deb"],
}


class Refused(Exception):
    pass


def version_key(version: str):
    m = VERSION.match(version)
    if not m:
        raise Refused(f"{version!r} isn't <x.y.z>-main.<YYYYMMDDHHMMSS>")
    return tuple(int(g) for g in m.groups())


def check_newer(version: str, previous) -> None:
    """The never-backwards check: refuses `version` unless it is newer than
    the manifest already published (`previous`, parsed JSON or None)."""
    version_key(version)
    if not previous:
        return
    old = previous.get("version", "") if isinstance(previous, dict) else ""
    try:
        older = version_key(old) < version_key(version)
    except Refused:
        older = True  # nothing comparable was published: anything starts the channel
    if not older:
        raise Refused(f"{version} isn't newer than the published {old}")


def base_url(app: str, repo: str = DEFAULT_REPO) -> str:
    if not APP.match(app):
        raise Refused(f"{app!r} isn't an app id (lower-case letters, digits and dashes)")
    if not REPO.match(repo):
        raise Refused(f"{repo!r} isn't <owner>/<repo>")
    return f"https://github.com/{repo}/releases/download/{app}-main"


def signature(path: Path, version: str) -> str:
    sig = path.with_name(path.name + ".sig")
    if not sig.is_file():
        raise Refused(f"{path.name} has no signature beside it")
    text = sig.read_text().strip()
    if not text:
        raise Refused(f"{sig.name} is empty")
    try:
        decoded = base64.b64decode(text, validate=True).decode()
    except Exception:
        raise Refused(f"{sig.name} isn't a minisign signature in base64") from None
    lines = decoded.splitlines()
    if len(lines) < 4 or not lines[0].startswith("untrusted comment:") or not lines[2].startswith("trusted comment:"):
        raise Refused(f"{sig.name} isn't a minisign signature")
    fields = dict(f.split(":", 1) for f in lines[2][len("trusted comment:"):].strip().split("\t") if ":" in f)
    if fields.get("version") != version:
        raise Refused(f"{sig.name} is signed for version {fields.get('version')!r}, not {version}")
    return text


def write(app, version, pub_date, bundles, notes, previous=None, repo=DEFAULT_REPO):
    url = base_url(app, repo)
    check_newer(version, previous)
    platforms = {}
    for kind, path in sorted(bundles):
        if kind not in PLATFORMS:
            raise Refused(f"no platform is called {kind!r}")
        if not path.is_file() or path.stat().st_size == 0:
            raise Refused(f"{path} is missing or empty")
        entry = {"signature": signature(path, version), "url": f"{url}/{quote(path.name)}"}
        for key in PLATFORMS[kind]:
            if key in platforms:
                raise Refused(f"two bundles were named for {kind}")
            platforms[key] = entry
    if not platforms:
        raise Refused("no bundle was named")
    return {"version": version, "notes": notes, "pub_date": pub_date, "platforms": dict(sorted(platforms.items()))}


def main(argv):
    p = argparse.ArgumentParser(description="Write an app's main-channel latest.json")
    p.add_argument("--app", required=True)
    p.add_argument("--repo", default=DEFAULT_REPO)
    p.add_argument("--version", required=True)
    p.add_argument("--pub-date", required=True)
    p.add_argument("--notes", default="")
    p.add_argument("--bundle", action="append", default=[], help="<darwin|windows|linux>=<file>")
    p.add_argument("--previous", type=Path, help="the published latest.json; a missing file means nothing is published")
    p.add_argument("--out", type=Path, required=True)
    a = p.parse_args(argv)
    bundles = []
    for b in a.bundle:
        kind, _, file = b.partition("=")
        bundles.append((kind, Path(file)))
    try:
        previous = None
        if a.previous and a.previous.is_file() and a.previous.stat().st_size:
            try:
                previous = json.loads(a.previous.read_text())
            except json.JSONDecodeError:
                raise Refused(f"the published manifest {a.previous} isn't JSON") from None
        manifest = write(a.app, a.version, a.pub_date, bundles, a.notes, previous, a.repo)
    except Refused as e:
        print(f"write-manifest: refused: {e}", file=sys.stderr)
        return 1
    a.out.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {a.out}: {a.version}, {len(manifest['platforms'])} platform keys")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
