import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
ARCHIVE_SCRIPT = SCRIPT_DIR / "archive-ios-app.sh"
PROJECT_PATH = SCRIPT_DIR.parent / "BerryKeepAppleFileProvider.xcodeproj"


class IosArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {
            **os.environ,
            "IOS_PROJECT_PATH": str(PROJECT_PATH),
            "IOS_APP_SCHEME": "BerryKeepIosApp",
            "IOS_DERIVED_DATA_PATH": str(self.root / "derived data"),
            "IOS_ARCHIVE_PATH": str(self.root / "app.xcarchive"),
            "IOS_BUILD_NUMBER": "123",
            "IOS_SIGNED_EXPORT_ENABLED": "true",
            "BERRYKEEP_IOS_SIGNING_IDENTITY": "Apple Distribution: Archive Test",
            "BERRYKEEP_IOS_DEVELOPMENT_TEAM": "TESTTEAM12",
            "BERRYKEEP_IOS_KEYCHAIN_PATH": str(self.root / "signing keychain"),
            "BERRYKEEP_IOS_APP_PROFILE_UUID": "app-profile",
            "BERRYKEEP_IOS_EXTENSION_PROFILE_UUID": "extension-profile",
        }

    def mock_archive(self):
        fake_bin = self.root / "bin"
        fake_bin.mkdir(exist_ok=True)
        xcodebuild = fake_bin / "xcodebuild"
        xcodebuild.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
        xcodebuild.chmod(0o755)
        return subprocess.run(
            ["sh", str(ARCHIVE_SCRIPT)],
            env={**self.env, "PATH": f"{fake_bin}:{os.environ['PATH']}"},
            capture_output=True,
            text=True,
        )

    def test_signed_archive_uses_manual_profiles_per_product(self):
        result = self.mock_archive()
        self.assertEqual(result.returncode, 0, result.stderr)
        args = result.stdout.splitlines()
        for setting in (
            "archive",
            "CODE_SIGN_STYLE=Manual",
            "CODE_SIGN_IDENTITY=Apple Distribution: Archive Test",
            "CODE_SIGNING_ALLOWED=YES",
            "CODE_SIGNING_REQUIRED=YES",
            "DEVELOPMENT_TEAM=TESTTEAM12",
            "CURRENT_PROJECT_VERSION=123",
            "PROVISIONING_PROFILE_SPECIFIER=$(BERRYKEEP_IOS_PROFILE_$(PRODUCT_NAME))",
            "BERRYKEEP_IOS_PROFILE_BerryKeepIosApp=app-profile",
            "BERRYKEEP_IOS_PROFILE_BerryKeepIosFileProviderExtension=extension-profile",
            f"OTHER_CODE_SIGN_FLAGS=--keychain {self.env['BERRYKEEP_IOS_KEYCHAIN_PATH']}",
            self.env["IOS_DERIVED_DATA_PATH"],
        ):
            self.assertIn(setting, args)
        self.assertNotIn("CODE_SIGN_STYLE=Automatic", args)

    def test_unsigned_archive_does_not_inherit_signing_inputs(self):
        self.env["IOS_SIGNED_EXPORT_ENABLED"] = "false"
        result = self.mock_archive()
        self.assertEqual(result.returncode, 0, result.stderr)
        args = result.stdout.splitlines()
        self.assertIn("CODE_SIGNING_ALLOWED=NO", args)
        self.assertIn("CODE_SIGNING_REQUIRED=NO", args)
        self.assertFalse(any(arg.startswith("PROVISIONING_PROFILE_SPECIFIER=") for arg in args))
        self.assertFalse(any(arg.startswith("CODE_SIGN_IDENTITY=") for arg in args))

    def test_signed_archive_requires_both_profiles(self):
        for key in ("BERRYKEEP_IOS_APP_PROFILE_UUID", "BERRYKEEP_IOS_EXTENSION_PROFILE_UUID"):
            with self.subTest(key=key):
                saved = self.env.pop(key)
                result = self.mock_archive()
                self.env[key] = saved
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(key, result.stderr)
                self.assertEqual(result.stdout, "")

    @unittest.skipUnless(shutil.which("xcodebuild"), "requires Xcode")
    def test_xcode_resolves_distinct_app_and_extension_profiles(self):
        result = subprocess.run(
            ["sh", str(ARCHIVE_SCRIPT), "-showBuildSettings", "-json"],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        settings = {item["target"]: item["buildSettings"] for item in json.loads(result.stdout)}
        for target, profile in (
            ("BerryKeepIosApp", "app-profile"),
            ("BerryKeepIosFileProviderExtension", "extension-profile"),
        ):
            with self.subTest(target=target):
                self.assertEqual(settings[target]["CODE_SIGN_STYLE"], "Manual")
                self.assertEqual(settings[target]["PROVISIONING_PROFILE_SPECIFIER"], profile)


if __name__ == "__main__":
    unittest.main()
