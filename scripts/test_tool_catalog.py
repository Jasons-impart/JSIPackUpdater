import importlib.util
import hashlib
import json
import pathlib
import tempfile
import sys
import types
import unittest
from unittest.mock import patch
sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("tool_catalog", pathlib.Path(__file__).with_name("tool-catalog.py"))
catalog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(catalog)

class CatalogTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.assets = pathlib.Path(self.temp.name)
        self.output = self.assets / "catalog.json"
        for target in catalog.TARGETS:
            filename = f"minecraft-modpack-updater-v0.1.0-{target}" + (".exe" if "windows" in target else "")
            binary = self.assets / filename
            binary.write_bytes(target.encode())
            digest = hashlib.sha256(binary.read_bytes()).hexdigest()
            binary.with_name(filename + ".sha256").write_text(digest + "  " + filename)
            self.write_bkmpw(target, "0.2.5")

    def write_bkmpw(self, target, version):
        entry = catalog.entry("bkmpw", version, target, "a" * 64, "0.1.0")
        (self.assets / f"bkmpw-{target}.json").write_text(json.dumps(entry))

    def generate(self):
        catalog.catalog(types.SimpleNamespace(assets=self.assets, version="0.1.0", output=self.output))
        return json.loads(self.output.read_text())

    def test_generate_and_refresh_preserves_all_updater_assets(self):
        original = self.generate()
        self.assertEqual(len(original["releases"]), 12)
        for target in catalog.TARGETS:
            self.write_bkmpw(target, "0.2.6")
        refreshed = self.assets / "refreshed.json"
        catalog.refresh(types.SimpleNamespace(assets=self.assets, existing=self.output, output=refreshed))
        entries = json.loads(refreshed.read_text())["releases"]
        self.assertEqual(entries[::2], original["releases"][::2])
        self.assertTrue(all(e["version"] == "0.2.6" for e in entries[1::2]))
        self.write_bkmpw(catalog.TARGETS[0], "0.2.7")
        with self.assertRaisesRegex(ValueError, "same tested version"):
            catalog.refresh(types.SimpleNamespace(assets=self.assets, existing=self.output, output=refreshed))

    def test_tampered_binary_and_nonofficial_entry_are_rejected(self):
        self.generate()
        binary = next(p for p in self.assets.iterdir() if p.name.startswith("minecraft") and not p.name.endswith(".sha256"))
        binary.write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            self.generate()
        entry_path = self.assets / f"bkmpw-{catalog.TARGETS[0]}.json"
        item = json.loads(entry_path.read_text())
        item["url"] = "https://example.com/unverified"
        entry_path.write_text(json.dumps(item))
        with self.assertRaisesRegex(ValueError, "invalid tested bkmpw entry"):
            catalog.refresh(types.SimpleNamespace(assets=self.assets, existing=self.output, output=self.output))

    def test_missing_arm_candidate_prevents_publication_of_a_partial_catalog(self):
        for target in ("aarch64-unknown-linux-gnu", "aarch64-pc-windows-msvc"):
            with self.subTest(target=target):
                record = self.assets / f"bkmpw-{target}.json"
                saved = record.read_bytes()
                record.unlink()
                with self.assertRaises(FileNotFoundError):
                    self.generate()
                self.assertFalse(self.output.exists())
                record.write_bytes(saved)

    def test_only_matching_published_stable_releases_can_be_recommended(self):
        metadata = self.assets / "release.json"
        args = types.SimpleNamespace(tag="v0.2.6", metadata=metadata)
        valid = dict(tagName=args.tag, isDraft=False, isPrerelease=False)
        metadata.write_text(json.dumps(valid))
        catalog.validate_release(args)
        for change in (dict(isDraft=True), dict(isPrerelease=True), dict(tagName="v0.2.5"), dict(isDraft=None)):
            with self.subTest(change=change):
                metadata.write_text(json.dumps(valid | change))
                with self.assertRaisesRegex(ValueError, "published stable"):
                    catalog.validate_release(args)

    def test_incompatible_protocol_and_hash_never_produce_tested_record(self):
        binary = self.assets / "bkmpw"
        binary.write_bytes(b"test")
        args = types.SimpleNamespace(binary=binary, sha256=hashlib.sha256(b"test").hexdigest(), version="0.2.5", target=catalog.TARGETS[0], output=self.assets / "tested.json")
        incompatible = b'{"event":"completed","ok":true,"data":{"protocolVersion":2,"cliVersion":"0.2.5"}}\n'
        with patch.object(catalog.subprocess, "check_output", return_value=incompatible):
            with self.assertRaisesRegex(ValueError, "incompatible"):
                catalog.verify_bkmpw(args)
        args.sha256 = "0" * 64
        with patch.object(catalog.subprocess, "check_output") as execute:
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                catalog.verify_bkmpw(args)
            execute.assert_not_called()
        self.assertFalse(args.output.exists())

    def test_tested_updater_version_is_the_compatibility_floor(self):
        binary = self.assets / "bkmpw"
        binary.write_bytes(b"test")
        args = types.SimpleNamespace(binary=binary, sha256=hashlib.sha256(b"test").hexdigest(), version="0.2.5", target=catalog.TARGETS[0], output=self.assets / "tested.json")
        completed = b'{"event":"completed","ok":true,"data":{"protocolVersion":1,"cliVersion":"0.2.5"}}\n'
        with patch.object(catalog.subprocess, "check_output", return_value=completed), patch.object(catalog, "tested_updater_version", return_value="0.2.0"):
            catalog.verify_bkmpw(args)
        self.assertEqual(json.loads(args.output.read_text())["minimumUpdater"], "0.2.0")
        with self.assertRaisesRegex(ValueError, "tested updater version"):
            catalog.entry("bkmpw", "0.2.5", catalog.TARGETS[0], "a" * 64)

if __name__ == "__main__":
    unittest.main()
