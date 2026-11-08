#!/usr/bin/env python3
"""Tests for the release scripts: the version stamp, the manifest writer and
its never-backwards check. Standard library only. From the repository root:

    python3 release/test_release.py
"""

import base64
import importlib.util
import json
import shutil
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), HERE / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


wm = load("write-manifest")
sv = load("stamp-version")

V1 = "0.1.0-main.20261107093000"
V2 = "0.1.0-main.20261107120000"
BASE = "https://github.com/snds/releases/releases/download/fontface-main"


def sig_text(version, file="bundle"):
    """A signature in the shape `tauri build` / `tauri signer sign --app-version` writes
    (the writer checks the shape and the version; the app checks the signature)."""
    comment = f"timestamp:1794017157\tfile:{file}"
    if version is not None:
        comment += f"\tversion:{version}"
    text = f"untrusted comment: signature from tauri secret key\nRUQAAAAA\ntrusted comment: {comment}\nAAAA\n"
    return base64.b64encode(text.encode()).decode()


class ManifestWriter(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir)

    def bundle(self, name, version=V1, sig=True):
        path = self.dir / name
        path.write_bytes(b"bundle bytes")
        if sig:
            (self.dir / f"{name}.sig").write_text(sig_text(version, name))
        return path

    def all_three(self, version=V1):
        return [
            ("darwin", self.bundle(f"Fontface_{version}_universal.app.tar.gz", version)),
            ("windows", self.bundle(f"Fontface_{version}_x64-setup.exe", version)),
            ("linux", self.bundle(f"fontface_{version}_amd64.deb", version)),
        ]

    def test_writes_every_platform_key_the_app_reads(self):
        m = wm.write("fontface", V1, "2026-11-07T09:30:00Z", self.all_three(), "notes")
        self.assertEqual(m["version"], V1)
        self.assertEqual(m["notes"], "notes")
        self.assertEqual(m["pub_date"], "2026-11-07T09:30:00Z")
        self.assertEqual(
            sorted(m["platforms"]),
            sorted(["darwin-aarch64", "darwin-x86_64", "darwin-aarch64-app", "darwin-x86_64-app",
                    "windows-x86_64", "windows-x86_64-nsis", "linux-x86_64", "linux-x86_64-deb"]),
        )
        mac = m["platforms"]["darwin-aarch64"]
        self.assertEqual(mac, m["platforms"]["darwin-x86_64"], "one universal bundle")
        self.assertEqual(mac["url"], f"{BASE}/Fontface_{V1}_universal.app.tar.gz")
        self.assertEqual(mac["signature"], sig_text(V1, f"Fontface_{V1}_universal.app.tar.gz"))
        self.assertTrue(m["platforms"]["windows-x86_64"]["url"].endswith("_x64-setup.exe"))
        self.assertEqual(json.loads(json.dumps(m)), m)
        self.assertEqual(list(m["platforms"]), sorted(m["platforms"]))

    def test_the_urls_point_at_the_apps_own_release(self):
        self.assertEqual(wm.base_url("vale"), "https://github.com/snds/releases/releases/download/vale-main")
        self.assertEqual(wm.base_url("vale", "someone/else"), "https://github.com/someone/else/releases/download/vale-main")
        m = wm.write("vale", V1, "", [("darwin", self.bundle("Vale x.app.tar.gz"))], "")
        self.assertEqual(m["platforms"]["darwin-aarch64"]["url"], "https://github.com/snds/releases/releases/download/vale-main/Vale%20x.app.tar.gz")

    def test_only_the_platforms_built_are_listed(self):
        m = wm.write("fontface", V1, "", self.all_three()[:2], "")
        self.assertNotIn("linux-x86_64", m["platforms"])
        self.assertIn("windows-x86_64", m["platforms"])

    def test_refuses_a_bad_app_id_or_repo(self):
        for bad in ["", "Fontface", "font face", "../x", "-x", "x/y"]:
            with self.assertRaises(wm.Refused, msg=bad):
                wm.write(bad, V1, "", self.all_three(), "")
        for bad in ["releases", "snds/releases/x", "https://github.com/snds/releases"]:
            with self.assertRaises(wm.Refused, msg=bad):
                wm.write("fontface", V1, "", self.all_three(), "", repo=bad)

    def test_refuses_a_version_outside_the_scheme(self):
        for bad in ["0.1.0", "0.1.0-main.2026110709", "v0.1.0-main.20261107093000", "0.1.0-beta.20261107093000"]:
            with self.assertRaises(wm.Refused, msg=bad):
                wm.write("fontface", bad, "", self.all_three(), "")

    def test_refuses_a_missing_empty_or_foreign_signature(self):
        with self.assertRaises(wm.Refused):
            wm.write("fontface", V1, "", [("darwin", self.bundle("a.app.tar.gz", sig=False))], "")
        empty = self.bundle("b.app.tar.gz")
        (self.dir / "b.app.tar.gz.sig").write_text("")
        with self.assertRaises(wm.Refused):
            wm.write("fontface", V1, "", [("darwin", empty)], "")
        junk = self.bundle("c.app.tar.gz")
        (self.dir / "c.app.tar.gz.sig").write_text("not base64 at all!")
        with self.assertRaises(wm.Refused):
            wm.write("fontface", V1, "", [("darwin", junk)], "")
        plain = self.bundle("c2.app.tar.gz")
        (self.dir / "c2.app.tar.gz.sig").write_text(base64.b64encode(b"just some text\n").decode())
        with self.assertRaisesRegex(wm.Refused, "isn't a minisign signature"):
            wm.write("fontface", V1, "", [("darwin", plain)], "")

    def test_refuses_a_signature_for_another_version_or_none(self):
        with self.assertRaisesRegex(wm.Refused, "signed for version"):
            wm.write("fontface", V2, "", [("darwin", self.bundle("d.app.tar.gz", version=V1))], "")
        with self.assertRaisesRegex(wm.Refused, "signed for version"):
            wm.write("fontface", V1, "", [("darwin", self.bundle("e.app.tar.gz", version=None))], "")

    def test_refuses_an_unknown_platform_an_empty_bundle_a_duplicate_or_none(self):
        with self.assertRaises(wm.Refused):
            wm.write("fontface", V1, "", [("android", self.bundle("f.apk"))], "")
        empty = self.bundle("g.exe")
        empty.write_bytes(b"")
        with self.assertRaises(wm.Refused):
            wm.write("fontface", V1, "", [("windows", empty)], "")
        with self.assertRaisesRegex(wm.Refused, "two bundles"):
            wm.write("fontface", V1, "", [("windows", self.bundle("h1.exe")), ("windows", self.bundle("h2.exe"))], "")
        with self.assertRaises(wm.Refused):
            wm.write("fontface", V1, "", [], "")

    def test_never_takes_the_channel_backwards(self):
        bundles = self.all_three(V1)
        with self.assertRaisesRegex(wm.Refused, "isn't newer"):
            wm.write("fontface", V1, "", bundles, "", previous={"version": V2})
        with self.assertRaisesRegex(wm.Refused, "isn't newer"):
            wm.write("fontface", V1, "", bundles, "", previous={"version": V1})
        with self.assertRaisesRegex(wm.Refused, "isn't newer"):
            wm.check_newer("0.1.0-main.20261107093000", {"version": "0.2.0-main.20200101000000"})
        # A higher base wins whatever the time; numbers compare as numbers.
        wm.check_newer("0.2.0-main.20200101000000", {"version": "0.1.9-main.20261107093000"})
        wm.check_newer("0.10.0-main.20200101000000", {"version": "0.9.0-main.20261107093000"})
        self.assertEqual(wm.write("fontface", V1, "", bundles, "", previous={"version": "0.1.0-main.20261101000000"})["version"], V1)
        # Nothing comparable published yet: the first run starts the channel.
        self.assertEqual(wm.write("fontface", V1, "", bundles, "", previous={})["version"], V1)
        self.assertEqual(wm.write("fontface", V1, "", bundles, "", previous={"version": "0.1.0"})["version"], V1)
        wm.check_newer(V1, None)
        with self.assertRaises(wm.Refused):
            wm.check_newer("0.1.0", None)

    def test_the_command_line_writes_the_file_and_fails_with_a_reason(self):
        out = self.dir / "latest.json"
        args = ["--app", "fontface", "--version", V1, "--pub-date", "2026-11-07T09:30:00Z", "--out", str(out)]
        bundles = sum((["--bundle", f"{k}={p}"] for k, p in self.all_three()), [])
        # A --previous that doesn't exist means nothing was published yet.
        self.assertEqual(wm.main(args + bundles + ["--previous", str(self.dir / "none.json")]), 0)
        self.assertEqual(json.loads(out.read_text())["version"], V1)
        previous = self.dir / "old.json"
        previous.write_text(json.dumps({"version": V2}))
        self.assertEqual(wm.main(args + bundles + ["--previous", str(previous)]), 1)
        previous.write_text("<html>not json</html>")
        self.assertEqual(wm.main(args + bundles + ["--previous", str(previous)]), 1)


def write(root, rel, text):
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


TAURI_CONF = '{\n  "productName": "App",\n  "version": "0.1.0",\n  "identifier": "design.snds.app"\n}\n'


def lock(*crates):
    out = "# This file is automatically @generated by Cargo.\nversion = 4\n"
    for name, version, source in crates:
        out += f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
        if source:
            out += 'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
    return out


class VersionStamp(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)

    def fontface(self):
        """One crate in src-tauri with its own Cargo.lock; npm at the root."""
        write(self.root, "package.json", '{\n  "name": "fontface",\n  "version": "0.1.0"\n}\n')
        write(self.root, "src-tauri/tauri.conf.json", TAURI_CONF)
        write(self.root, "src-tauri/Cargo.toml", '[package]\nname = "fontface"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nserde = "1"\n')
        write(self.root, "src-tauri/Cargo.lock", lock(("fontface", "0.1.0", False), ("serde", "0.1.0", True)))

    def vale(self, inherit=False):
        """A workspace at the root; the app crate in apps/web/src-tauri."""
        write(self.root, "Cargo.toml", '[workspace]\nmembers = ["crates/core", "apps/web/src-tauri"]\n\n[workspace.package]\n'
              + ('version = "0.1.0"\n' if inherit else "") + 'edition = "2024"\n')
        write(self.root, "package.json", '{\n  "version": "0.0.0"\n}\n')
        write(self.root, "apps/web/package.json", '{\n  "version": "0.0.0"\n}\n')
        write(self.root, "apps/web/src-tauri/tauri.conf.json", TAURI_CONF)
        version = "version.workspace = true" if inherit else 'version = "0.1.0"'
        write(self.root, "apps/web/src-tauri/Cargo.toml", f'[package]\nname = "vale"\n{version}\n\n[dependencies]\npim-core = {{ path = "../../../crates/core", version = "0.1.0" }}\n')
        core_version = "version.workspace = true" if inherit else 'version = "0.1.0"'
        write(self.root, "crates/core/Cargo.toml", f'[package]\nname = "pim-core"\n{core_version}\n')
        write(self.root, "Cargo.lock", lock(("pim-core", "0.1.0", False), ("vale", "0.1.0", False), ("serde", "0.1.0", True)))

    def test_a_single_crate_app(self):
        self.fontface()
        before = (self.root / "src-tauri/tauri.conf.json").read_text().splitlines()
        changed = sv.stamp(self.root, "src-tauri", V1)
        self.assertEqual(changed, ["src-tauri/tauri.conf.json", "src-tauri/Cargo.toml", "src-tauri/Cargo.lock (1 crates)", "package.json"])
        self.assertEqual(json.loads((self.root / "src-tauri/tauri.conf.json").read_text())["version"], V1)
        self.assertEqual(json.loads((self.root / "package.json").read_text())["version"], V1)
        self.assertIn(f'name = "fontface"\nversion = "{V1}"', (self.root / "src-tauri/Cargo.toml").read_text())
        lockfile = (self.root / "src-tauri/Cargo.lock").read_text()
        self.assertIn(f'name = "fontface"\nversion = "{V1}"', lockfile)
        self.assertIn('name = "serde"\nversion = "0.1.0"\nsource', lockfile, "registry crates keep their versions")
        after = (self.root / "src-tauri/tauri.conf.json").read_text().splitlines()
        self.assertEqual([a for a, b in zip(before, after) if a != b], ['  "version": "0.1.0",'], "only the version line changes")

    def test_an_app_crate_in_a_workspace_stamps_only_itself(self):
        self.vale()
        sv.stamp(self.root, "apps/web/src-tauri", V1)
        self.assertEqual(json.loads((self.root / "apps/web/src-tauri/tauri.conf.json").read_text())["version"], V1)
        self.assertIn(f'version = "{V1}"', (self.root / "apps/web/src-tauri/Cargo.toml").read_text())
        lockfile = (self.root / "Cargo.lock").read_text()
        self.assertIn(f'name = "vale"\nversion = "{V1}"', lockfile)
        self.assertIn('name = "pim-core"\nversion = "0.1.0"\n', lockfile)
        # The app's own dependency on pim-core keeps its requirement.
        self.assertIn('pim-core = { path = "../../../crates/core", version = "0.1.0" }', (self.root / "apps/web/src-tauri/Cargo.toml").read_text())
        self.assertEqual(json.loads((self.root / "apps/web/package.json").read_text())["version"], "0.0.0")
        self.assertNotIn(V1, (self.root / "crates/core/Cargo.toml").read_text())

    def test_a_workspace_inherited_version_moves_every_inheriting_crate(self):
        self.vale(inherit=True)
        sv.stamp(self.root, "apps/web/src-tauri", V1)
        self.assertIn(f'version = "{V1}"', (self.root / "Cargo.toml").read_text())
        lockfile = (self.root / "Cargo.lock").read_text()
        self.assertIn(f'name = "vale"\nversion = "{V1}"', lockfile)
        self.assertIn(f'name = "pim-core"\nversion = "{V1}"', lockfile)
        self.assertIn('name = "serde"\nversion = "0.1.0"\nsource', lockfile)
        self.assertIn(f'pim-core = {{ path = "../../../crates/core", version = "{V1}" }}', (self.root / "apps/web/src-tauri/Cargo.toml").read_text())

    def test_refuses_another_scheme_another_base_or_a_second_stamp(self):
        self.fontface()
        for bad in ["0.1.0", "0.2.0-main.20261107093000", "0.1.0-main.1"]:
            with self.assertRaises(SystemExit, msg=bad):
                sv.stamp(self.root, "src-tauri", bad)
        sv.stamp(self.root, "src-tauri", V1)
        with self.assertRaises(SystemExit):
            sv.stamp(self.root, "src-tauri", V2)

    def test_refuses_a_missing_tauri_dir_or_a_cargo_version_that_disagrees(self):
        self.fontface()
        with self.assertRaises(SystemExit):
            sv.stamp(self.root, "nowhere", V1)
        write(self.root, "src-tauri/Cargo.toml", '[package]\nname = "fontface"\nversion = "0.0.9"\n')
        with self.assertRaisesRegex(SystemExit, "Cargo.toml's version"):
            sv.stamp(self.root, "src-tauri", V1)


if __name__ == "__main__":
    unittest.main()
