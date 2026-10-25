#!/usr/bin/env python3
"""Package and verify the static Rendezvous artifact without executing signing inputs."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib

BINARY = "berrykeep-rendezvous-service"
TARGET = "x86_64-unknown-linux-musl"
VERSION = r"[1-9][0-9]*\.[0-9]+\.[0-9]+"


def validate_toolchain_channel(channel):
    """Allow only a single Rustup channel token before output/action interpolation."""
    pattern = r"(?:stable|beta|nightly)(?:-[0-9]{4}-[0-9]{2}-[0-9]{2})?|[1-9][0-9]*\.[0-9]+(?:\.[0-9]+)?"
    if not isinstance(channel, str) or not re.fullmatch(pattern, channel):
        raise ValueError("Unexpected Rust toolchain channel")
    return channel


def validate_build_revision(revision):
    """The describe label is informational; commit/tag identify release source."""
    if not isinstance(revision, str) or not 1 <= len(revision) <= 1024 or any(not char.isprintable() or char.isspace() for char in revision):
        raise ValueError("Unexpected build revision label")
    return revision


def command(args, cwd=None):
    result = subprocess.run(args, cwd=cwd, text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise ValueError(f"Command failed ({args[0]}): {result.stderr.strip()}")
    return result.stdout.strip()


def validate_release_identity(source, expected_commit, tag, version):
    git = lambda *args: command(["git", "-C", str(source), *args])
    if not re.fullmatch(VERSION, version):
        raise ValueError("Workspace version must be a stable version")
    if tag is not None:
        if tag != "v" + version:
            raise ValueError("Release tag and workspace version disagree")
        if git("cat-file", "-t", tag) != "tag":
            raise ValueError("Release tag must be annotated")
        if git("rev-parse", tag + "^{commit}") != expected_commit:
            raise ValueError("Release tag does not identify the source commit")
        reachable = subprocess.run(["git", "-C", str(source), "merge-base", "--is-ancestor", expected_commit, "refs/remotes/origin/main"], capture_output=True)
        if reachable.returncode:
            raise ValueError("Release source must be reachable from origin/main")


def validate_dispatch_context(source, event_name, ref):
    """Keep main repairs and existing stable-tag callers, rejecting other refs."""
    if event_name != "workflow_dispatch" or ref == "refs/heads/main":
        return
    if not ref.startswith("refs/tags/") or not re.fullmatch("v" + VERSION, ref[len("refs/tags/"):]):
        raise ValueError("Manual publication requires main or a validated stable release tag")
    tag = ref[len("refs/tags/"):]
    git = lambda *args: command(["git", "-C", str(source), *args])
    commit = git("rev-parse", tag + "^{commit}")
    version = tomllib.loads(git("show", commit + ":Cargo.toml"))["workspace"]["package"]["version"]
    validate_release_identity(source, commit, tag, version)


def validate_source(source, expected_commit, tag):
    source = Path(source)
    git = lambda *args: command(["git", "-C", str(source), *args])
    if not re.fullmatch(r"[0-9a-f]{40}", expected_commit) or git("rev-parse", "HEAD") != expected_commit:
        raise ValueError("Source commit does not match the validated commit")
    if git("status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("Release source must be clean, including untracked files")
    version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    validate_release_identity(source, expected_commit, tag, version)
    revision = validate_build_revision(git("describe", "--tags", "--always", "--dirty=-dirty", "--abbrev=12"))
    return version, revision, int(git("show", "-s", "--format=%ct", "HEAD"))


def prepare_source(source, expected_commit, tag=None):
    """Resolve only approved source from a trusted initial checkout before building."""
    source = Path(source)
    git = lambda *args: command(["git", "-C", str(source), *args])
    if not re.fullmatch(r"[0-9a-f]{40}", expected_commit):
        raise ValueError("Expected an immutable source commit")
    if git("status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("Initial source checkout must be clean")
    available = subprocess.run(["git", "-C", str(source), "cat-file", "-e", expected_commit], capture_output=True)
    if available.returncode:
        git("fetch", "--no-tags", "origin", expected_commit)
    if git("cat-file", "-t", expected_commit) != "commit":
        raise ValueError("Expected a source commit object")
    version = tomllib.loads(git("show", expected_commit + ":Cargo.toml"))["workspace"]["package"]["version"]
    validate_release_identity(source, expected_commit, tag, version)
    git("checkout", "--detach", expected_commit)
    return validate_source(source, expected_commit, tag)


def validate_static_elf(data):
    if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01" or int.from_bytes(data[18:20], "little") != 62:
        raise ValueError("Artifact must contain a little-endian x86_64 ELF executable")
    with tempfile.TemporaryDirectory() as temporary:
        binary = Path(temporary) / BINARY
        binary.write_bytes(data)
        environment = dict(os.environ, LC_ALL="C")
        for option, marker in [("--program-headers", "INTERP"), ("--dynamic", "(NEEDED)")]:
            result = subprocess.run(["readelf", "--wide", option, str(binary)], env=environment, text=True, capture_output=True, check=True)
            if marker in result.stdout:
                raise ValueError("Release binary has a dynamic interpreter or dependency")


def validate_binary_version(data, version, revision):
    with tempfile.TemporaryDirectory() as temporary:
        binary = Path(temporary) / BINARY
        binary.write_bytes(data)
        binary.chmod(0o755)
        if command([str(binary), "--version"]).splitlines() != [f"{BINARY} {version}", f"Build revision: {revision}"]:
            raise ValueError("Binary version/revision does not match its source")


def artifact_name(version):
    if not re.fullmatch(VERSION, version):
        raise ValueError("Invalid release version")
    return f"{BINARY}-{version}-{TARGET}"


def package_release(source, binary, output, expected_commit, tag=None):
    version, revision, epoch = validate_source(source, expected_commit, tag)
    data = Path(binary).read_bytes()
    validate_static_elf(data)
    validate_binary_version(data, version, revision)
    binary_hash = hashlib.sha256(data).hexdigest()
    metadata = {"schema_version": 1, "binary": BINARY, "version": version, "release_tag": tag, "git_commit": expected_commit, "build_revision": revision, "rust_target": TARGET, "source_dirty": False, "binary_sha256": binary_hash}
    basename = artifact_name(version)
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    archive = output / (basename + ".tar.gz")
    members = [(BINARY, data, 0o755), ("SHA256SUMS", f"{binary_hash}  {BINARY}\n".encode(), 0o644), ("build-metadata.json", (json.dumps(metadata, indent=2, sort_keys=True) + "\n").encode(), 0o644)]
    with archive.open("wb") as raw:
        with gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as tar:
                for name, contents, mode in members:
                    info = tarfile.TarInfo(basename + "/" + name)
                    info.size, info.mode, info.mtime = len(contents), mode, epoch
                    tar.addfile(info, io.BytesIO(contents))
    archive.with_name(archive.name + ".sha256").write_text(hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + archive.name + "\n")
    return archive


def verify_archive(archive, tag, expected_commit):
    """Verify downloaded build inputs before signing; never execute their binary."""
    if not re.fullmatch("v" + VERSION, tag) or not re.fullmatch(r"[0-9a-f]{40}", expected_commit):
        raise ValueError("Invalid expected tag/commit")
    basename = artifact_name(tag[1:])
    archive = Path(archive)
    if archive.name != basename + ".tar.gz":
        raise ValueError("Unexpected archive name")
    expected_names = {basename + "/" + name for name in [BINARY, "SHA256SUMS", "build-metadata.json"]}
    with tarfile.open(archive, "r:gz") as tar:
        members = tar.getmembers()
        if len(members) != 3 or {member.name for member in members} != expected_names:
            raise ValueError("Unexpected or duplicate archive entries")
        if any(not member.isfile() or member.size > 128 * 1024 * 1024 for member in members):
            raise ValueError("Archive entries must be bounded regular files")
        if any(member.mode != (0o755 if member.name.endswith("/" + BINARY) else 0o644) for member in members):
            raise ValueError("Archive permissions must be 0755 for the binary and 0644 for metadata")
        contents = {member.name.rsplit("/", 1)[1]: tar.extractfile(member).read() for member in members}
    metadata = json.loads(contents["build-metadata.json"])
    if metadata.get("git_commit") != expected_commit:
        raise ValueError("Archive source commit does not match the validated commit")
    if metadata.get("release_tag") != tag:
        raise ValueError("Archive release tag does not match the validated tag")
    if any(metadata.get(key) != value for key, value in {"schema_version": 1, "binary": BINARY, "version": tag[1:], "rust_target": TARGET, "source_dirty": False}.items()):
        raise ValueError("Archive metadata does not describe the expected release")
    validate_build_revision(metadata.get("build_revision"))
    digest = hashlib.sha256(contents[BINARY]).hexdigest()
    if metadata.get("binary_sha256") != digest or contents["SHA256SUMS"] != f"{digest}  {BINARY}\n".encode():
        raise ValueError("Archive binary hash mismatch")
    validate_static_elf(contents[BINARY])
    return metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare")
    prepare.add_argument("--source", type=Path, required=True)
    prepare.add_argument("--commit", required=True)
    prepare.add_argument("--tag")
    package = commands.add_parser("package")
    package.add_argument("--source", type=Path, required=True)
    package.add_argument("--binary", type=Path, required=True)
    package.add_argument("--output", type=Path, required=True)
    package.add_argument("--commit", required=True)
    package.add_argument("--tag")
    verify = commands.add_parser("verify")
    verify.add_argument("--archive", type=Path, required=True)
    verify.add_argument("--tag", required=True)
    verify.add_argument("--commit", required=True)
    args = parser.parse_args()
    if args.command == "prepare":
        prepare_source(args.source, args.commit, args.tag)
    elif args.command == "package":
        print(package_release(args.source, args.binary, args.output, args.commit, args.tag))
    else:
        print(json.dumps(verify_archive(args.archive, args.tag, args.commit)))


if __name__ == "__main__":
    main()
