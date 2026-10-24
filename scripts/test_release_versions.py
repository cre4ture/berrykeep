"""Regression checks for release version consistency across Rust and Apple builds."""

from pathlib import Path
import shutil
import tempfile
import unittest

from validate_release_versions import validate_release_versions


class ReleaseVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.apple = self.root / "apps/apple-file-provider"
        (self.apple / "BerryKeepAppleFileProvider.xcodeproj").mkdir(parents=True)
        self.write_versions()

    def write_versions(self, workspace="2.1.0", spec="2.1.0", project=("2.1.0", "2.1.0")):
        (self.root / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{workspace}"\n')
        (self.apple / "project.yml").write_text(f"settings:\n  base:\n    MARKETING_VERSION: {spec}\n")
        (self.apple / "BerryKeepAppleFileProvider.xcodeproj/project.pbxproj").write_text(
            "\n".join(f"    MARKETING_VERSION = {version};" for version in project)
        )

    def test_matching_versions_pass(self):
        self.assertEqual(validate_release_versions(self.root), "2.1.0")

    def test_version_is_derived_from_the_workspace(self):
        self.write_versions(workspace="3.2.1", spec="3.2.1", project=("3.2.1", "3.2.1"))
        self.assertEqual(validate_release_versions(self.root), "3.2.1")

    def test_accepts_quoted_versions_and_yaml_comments(self):
        self.write_versions(spec='"2.1.0" # release version', project=('"2.1.0"', "2.1.0"))
        self.assertEqual(validate_release_versions(self.root), "2.1.0")

    def test_rejects_a_quoted_value_with_a_matching_version_prefix(self):
        self.write_versions(spec='"2.1.0 extra"')
        with self.assertRaisesRegex(ValueError, "project.yml"):
            validate_release_versions(self.root)

    def test_rejects_the_previous_release_drift(self):
        self.write_versions(workspace="2.0.1", spec="2.0.0", project=("2.0.0", "2.0.0"))
        with self.assertRaisesRegex(ValueError, "project.yml"):
            validate_release_versions(self.root)

    def test_rejects_one_stale_project_configuration(self):
        self.write_versions(project=("2.1.0", "2.0.1"))
        with self.assertRaisesRegex(ValueError, "project.pbxproj"):
            validate_release_versions(self.root)

    def test_rejects_missing_marketing_version(self):
        self.write_versions(project=())
        with self.assertRaisesRegex(ValueError, "project.pbxproj"):
            validate_release_versions(self.root)

    def test_rejects_drift_in_an_additional_spec(self):
        other = self.root / "apps/another-apple-app"
        other.mkdir()
        (other / "project.yml").write_text("MARKETING_VERSION: 2.0.0\n")
        with self.assertRaisesRegex(ValueError, "another-apple-app/project.yml"):
            validate_release_versions(self.root)

    def test_rejects_drift_in_an_additional_project(self):
        other = self.root / "apps/another-apple-app/Other.xcodeproj"
        other.mkdir(parents=True)
        (other / "project.pbxproj").write_text("MARKETING_VERSION = 2.0.0;\n")
        with self.assertRaisesRegex(ValueError, "Other.xcodeproj/project.pbxproj"):
            validate_release_versions(self.root)

    def test_rejects_missing_apple_metadata(self):
        shutil.rmtree(self.apple)
        with self.assertRaisesRegex(ValueError, "no Apple version metadata found"):
            validate_release_versions(self.root)


if __name__ == "__main__":
    unittest.main()
