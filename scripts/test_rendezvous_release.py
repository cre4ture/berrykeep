"""Exercise release provenance and archive checks using real static/dynamic ELF files."""

import hashlib
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from rendezvous_release import package_release, verify_archive
from publish_rendezvous_release import publish, verify_signature


class RendezvousReleaseTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not shutil.which("musl-gcc"):
            raise RuntimeError("Release regression tests require musl-gcc")
        cls.fixtures = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.fixtures.cleanup)
        root = Path(cls.fixtures.name)
        cls.static = cls.compile_fixture(root, "static", "2.1.0", ["musl-gcc", "-static"])
        cls.dynamic = cls.compile_fixture(root, "dynamic", "2.1.0", ["cc"])
        cls.stale = cls.compile_fixture(root, "stale", "2.0.1", ["musl-gcc", "-static"])

    @staticmethod
    def compile_fixture(root, name, version, compiler):
        source = root / (name + ".c")
        message = f"berrykeep-rendezvous-service {version}\nBuild revision: v{version}"
        source.write_text("#include <stdio.h>\nint main(void) { puts(" + json.dumps(message) + "); return 0; }\n")
        binary = root / name
        subprocess.run(compiler + [str(source), "-o", str(binary)], check=True)
        return binary

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / "source"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@example.invalid")
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "2.1.0"\n')
        self.git("add", ".")
        self.git("commit", "-qm", "Test source")
        self.commit = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.commit)
        self.git("tag", "-a", "v2.1.0", "-m", "Release v2.1.0")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()

    def package(self, binary=None, tag="v2.1.0", output="out"):
        return package_release(self.repo, binary or self.static, self.root / output, self.commit, tag)

    def test_exact_clean_release_round_trip(self):
        archive = self.package()
        metadata = verify_archive(archive, "v2.1.0", self.commit)
        self.assertEqual(metadata["git_commit"], self.commit)
        self.assertEqual(metadata["version"], "2.1.0")
        self.assertEqual(metadata["rust_target"], "x86_64-unknown-linux-musl")
        self.assertEqual(metadata["binary_sha256"], hashlib.sha256(self.static.read_bytes()).hexdigest())
        self.assertEqual(archive.with_name(archive.name + ".sha256").read_text(), hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + archive.name + "\n")

    def test_rejects_modified_tracked_source(self):
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "2.1.0"\n# changed\n')
        with self.assertRaisesRegex(ValueError, "clean"):
            self.package()

    def test_rejects_untracked_source(self):
        (self.repo / "unexpected.rs").write_text("// untracked")
        with self.assertRaisesRegex(ValueError, "clean"):
            self.package()

    def test_rejects_lightweight_tag(self):
        self.git("tag", "-d", "v2.1.0")
        self.git("tag", "v2.1.0")
        with self.assertRaisesRegex(ValueError, "annotated"):
            self.package()

    def test_rejects_version_tag_mismatch(self):
        with self.assertRaisesRegex(ValueError, "version"):
            self.package(tag="v2.0.1")

    def test_rejects_wrong_expected_source_commit(self):
        with self.assertRaisesRegex(ValueError, "commit"):
            package_release(self.repo, self.static, self.root / "out", "0" * 40, "v2.1.0")

    def test_rejects_source_outside_main(self):
        (self.repo / "later.txt").write_text("later")
        self.git("add", ".")
        self.git("commit", "-qm", "Unmerged source")
        self.commit = self.git("rev-parse", "HEAD")
        self.git("tag", "-f", "-a", "v2.1.0", "-m", "Unmerged release")
        with self.assertRaisesRegex(ValueError, "main"):
            self.package()

    def test_rejects_dynamic_elf(self):
        with self.assertRaisesRegex(ValueError, "dynamic"):
            self.package(binary=self.dynamic)

    def test_rejects_another_elf_architecture(self):
        changed = bytearray(self.static.read_bytes())
        changed[18:20] = (183).to_bytes(2, "little")
        binary = self.root / "wrong-machine"
        binary.write_bytes(changed)
        with self.assertRaisesRegex(ValueError, "x86_64"):
            self.package(binary=binary)

    def test_rejects_stale_binary_version(self):
        with self.assertRaisesRegex(ValueError, "version"):
            self.package(binary=self.stale)

    def test_archive_is_reproducible(self):
        first = self.package(output="first")
        second = self.package(output="second")
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_development_build_cannot_be_verified_as_a_release(self):
        archive = self.package(tag=None)
        with self.assertRaisesRegex(ValueError, "tag"):
            verify_archive(archive, "v2.1.0", self.commit)

    def rewrite_archive(self, archive, transform):
        with tarfile.open(archive, "r:gz") as source:
            members = [(member, source.extractfile(member).read()) for member in source.getmembers()]
        with tarfile.open(archive, "w:gz") as target:
            for member, data in members:
                member, data = transform(member, data)
                member.size = len(data) if member.isfile() else 0
                target.addfile(member, io.BytesIO(data) if member.isfile() else None)

    def test_rejects_tampered_source_metadata(self):
        archive = self.package()
        def tamper(member, data):
            if member.name.endswith("build-metadata.json"):
                body = json.loads(data)
                body["git_commit"] = "0" * 40
                data = json.dumps(body).encode()
            return member, data
        self.rewrite_archive(archive, tamper)
        with self.assertRaisesRegex(ValueError, "commit"):
            verify_archive(archive, "v2.1.0", self.commit)

    def test_rejects_tampered_binary(self):
        archive = self.package()
        def tamper(member, data):
            if member.name.endswith("/berrykeep-rendezvous-service"):
                data += b"changed"
            return member, data
        self.rewrite_archive(archive, tamper)
        with self.assertRaisesRegex(ValueError, "hash"):
            verify_archive(archive, "v2.1.0", self.commit)

    def test_rejects_archive_links(self):
        archive = self.package()
        def tamper(member, data):
            if member.name.endswith("/berrykeep-rendezvous-service"):
                member.type = tarfile.SYMTYPE
                member.linkname = "/usr/bin/true"
            return member, data
        self.rewrite_archive(archive, tamper)
        with self.assertRaisesRegex(ValueError, "regular"):
            verify_archive(archive, "v2.1.0", self.commit)

    def test_rejects_setuid_archive_permissions(self):
        archive = self.package()
        def tamper(member, data):
            if member.name.endswith("/berrykeep-rendezvous-service"):
                member.mode = 0o4755
            return member, data
        self.rewrite_archive(archive, tamper)
        with self.assertRaisesRegex(ValueError, "permissions"):
            verify_archive(archive, "v2.1.0", self.commit)

    def test_publication_rejects_conflicting_existing_asset_without_uploading(self):
        archive = self.package()
        calls = []
        def github(*args, **kwargs):
            calls.append(args)
            if args[:2] == ("release", "view"):
                return subprocess.CompletedProcess(args, 0, json.dumps({"tagName": "v2.1.0", "isDraft": False, "isPrerelease": False, "url": "https://example.invalid/release", "assets": [{"name": archive.name}]}), "")
            if args[:2] == ("release", "download"):
                destination = Path(args[args.index("--dir") + 1])
                (destination / archive.name).write_bytes(b"different published bytes")
                return subprocess.CompletedProcess(args, 0, "", "")
            self.fail("Unexpected GitHub mutation: " + str(args))
        with patch("publish_rendezvous_release.gh", side_effect=github):
            with self.assertRaisesRegex(ValueError, "different bytes"):
                publish("example/repo", "v2.1.0", self.commit, archive.parent, self.root, "0" * 40)
        self.assertFalse(any(args[:2] == ("release", "upload") for args in calls))


class SignatureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.home = Path(cls.temporary.name)
        cls.home.chmod(0o700)
        cls.gpg = ["gpg", "--homedir", str(cls.home), "--batch"]
        subprocess.run(cls.gpg + ["--pinentry-mode", "loopback", "--passphrase", "", "--quick-generate-key", "Release fixture <fixture@example.invalid>", "ed25519", "sign", "1d"], check=True, capture_output=True)
        cls.addClassCleanup(subprocess.run, ["gpgconf", "--homedir", str(cls.home), "--kill", "gpg-agent"], capture_output=True)
        listing = subprocess.check_output(cls.gpg + ["--with-colons", "--list-keys"], text=True, stderr=subprocess.DEVNULL)
        cls.fingerprint = next(line.split(":")[9] for line in listing.splitlines() if line.startswith("fpr:"))

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.checksum = Path(temporary.name) / "release.sha256"
        self.checksum.write_text("a" * 64 + "  release.tar.gz\n")
        self.signature = self.checksum.with_suffix(".asc")
        subprocess.run(self.gpg + ["--armor", "--detach-sign", "--output", str(self.signature), str(self.checksum)], check=True, capture_output=True)

    def test_accepts_the_expected_signer(self):
        verify_signature(self.signature, self.checksum, self.home, self.fingerprint)

    def test_rejects_another_signer_pin(self):
        with self.assertRaisesRegex(ValueError, "signer"):
            verify_signature(self.signature, self.checksum, self.home, "0" * 40)

    def test_rejects_a_changed_checksum(self):
        self.checksum.write_text("b" * 64 + "  release.tar.gz\n")
        with self.assertRaisesRegex(ValueError, "signature"):
            verify_signature(self.signature, self.checksum, self.home, self.fingerprint)


if __name__ == "__main__":
    unittest.main()
