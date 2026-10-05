import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("publish_release", pathlib.Path(__file__).with_name("publish-release.py"))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)

class PublishTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.assets = pathlib.Path(temp.name)
        (self.assets / "binary").write_bytes(b"verified")
        self.args = types.SimpleNamespace(tag="v0.1.0", repo="test/updater", assets=self.assets)

    def test_public_release_only_reads_and_rejects_changed_or_missing_assets(self):
        for content in [b"verified", b"changed"]:
            calls = []
            def gh(*args, **kwargs):
                calls.append(args)
                if args[0] == "api":
                    return json.dumps({"draft": False, "assets": [{"name": "binary"}]})
                self.assertEqual(args[:2], ("release", "download"))
                (pathlib.Path(args[-1]) / "binary").write_bytes(content)
            with patch.object(publisher, "gh", side_effect=gh):
                if content == b"verified":
                    publisher.publish(self.args)
                else:
                    with self.assertRaisesRegex(ValueError, "public artifact differs"):
                        publisher.publish(self.args)
            self.assertTrue(all(call[0] == "api" or call[:2] == ("release", "download") for call in calls))
        with patch.object(publisher, "gh", return_value=json.dumps({"draft": False, "assets": []})) as gh:
            with self.assertRaisesRegex(ValueError, "incomplete"):
                publisher.publish(self.args)
            self.assertEqual(gh.call_count, 1)

    def test_draft_upload_failure_never_publishes_then_retry_completes(self):
        calls = []
        def interrupted(*args, **kwargs):
            calls.append(args)
            if args[0] == "api": return '{"draft": true}'
            raise subprocess.CalledProcessError(1, "gh")
        with patch.object(publisher, "gh", side_effect=interrupted):
            with self.assertRaises(subprocess.CalledProcessError): publisher.publish(self.args)
        self.assertFalse(any(call[:2] == ("release", "edit") for call in calls))
        with patch.object(publisher, "gh", side_effect=['{"draft": true}', "", ""]) as gh:
            publisher.publish(self.args)
            self.assertEqual(gh.call_args.args[:2], ("release", "edit"))

    def test_registry_authentication_failure_never_creates_a_draft(self):
        with patch.object(publisher, "gh", side_effect=subprocess.CalledProcessError(1, "gh", stderr="HTTP 401")) as gh:
            with self.assertRaises(subprocess.CalledProcessError): publisher.publish(self.args)
            self.assertEqual(gh.call_count, 1)

if __name__ == "__main__": unittest.main()
