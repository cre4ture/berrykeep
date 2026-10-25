#!/usr/bin/env python3
"""Append signed Rendezvous assets to a release, preserving existing published bytes."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from rendezvous_release import artifact_name, verify_archive


def gh(*args, check=True):
    return subprocess.run(["gh", *args], text=True, capture_output=True, check=check, timeout=180)


def verify_signature(signature, checksum, home, fingerprint):
    result = subprocess.run(["gpg", "--homedir", str(home), "--batch", "--status-fd", "1", "--verify", str(signature), str(checksum)], text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise ValueError("Invalid release checksum signature")
    if not any(line.startswith("[GNUPG:] VALIDSIG ") and fingerprint in line.split() for line in result.stdout.splitlines()):
        raise ValueError("Release checksum signer does not match the archive key fingerprint")


def release_metadata(repo, tag):
    result = gh("release", "view", tag, "--repo", repo, "--json", "tagName,isDraft,isPrerelease,assets,url", check=False)
    return json.loads(result.stdout) if result.returncode == 0 else None


def ensure_release(repo, tag):
    metadata = release_metadata(repo, tag)
    if metadata is None:
        result = gh("release", "create", tag, "--repo", repo, "--verify-tag", "--draft", "--title", "BerryKeep " + tag[1:], "--generate-notes", check=False)
        metadata = release_metadata(repo, tag)
        if metadata is None:
            raise ValueError("Cannot create or read release: " + result.stderr.strip())
    if metadata["tagName"] != tag or metadata["isPrerelease"]:
        raise ValueError("Expected the exact stable release tag")
    return metadata


def publish(repo, tag, commit, directory, home, fingerprint):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("Invalid GitHub repository")
    fingerprint = "".join(fingerprint.split()).upper()
    if not re.fullmatch(r"[0-9A-F]{40}", fingerprint):
        raise ValueError("Expected the full archive key fingerprint")
    directory, home = Path(directory).resolve(), Path(home).resolve()
    archive = directory / (artifact_name(tag[1:]) + ".tar.gz")
    verify_archive(archive, tag, commit)
    checksum = archive.with_name(archive.name + ".sha256")
    expected_checksum = hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + archive.name + "\n"
    if checksum.read_text() != expected_checksum:
        raise ValueError("Archive checksum does not describe the verified artifact")
    signature = checksum.with_name(checksum.name + ".asc")
    metadata = ensure_release(repo, tag)
    existing_names = {asset["name"] for asset in metadata["assets"]}
    uploads = []
    with tempfile.TemporaryDirectory() as temporary:
        existing = Path(temporary)
        for payload in [archive, checksum]:
            if payload.name in existing_names:
                gh("release", "download", tag, "--repo", repo, "--pattern", payload.name, "--dir", str(existing))
                if (existing / payload.name).read_bytes() != payload.read_bytes():
                    raise ValueError("Existing release asset has different bytes: " + payload.name)
            else:
                uploads.append(payload)
        if signature.name in existing_names:
            gh("release", "download", tag, "--repo", repo, "--pattern", signature.name, "--dir", str(existing))
            verify_signature(existing / signature.name, checksum, home, fingerprint)
        else:
            result = subprocess.run(["gpg", "--homedir", str(home), "--batch", "--yes", "--pinentry-mode", "loopback", "--passphrase-fd", "0", "--local-user", fingerprint, "--digest-algo", "SHA256", "--armor", "--detach-sign", "--output", str(signature), str(checksum)], input=os.environ["APT_REPO_GPG_PASSPHRASE"] + "\n", text=True, capture_output=True, timeout=30)
            if result.returncode:
                raise ValueError("Release checksum signing failed")
            verify_signature(signature, checksum, home, fingerprint)
            uploads.append(signature)
        if uploads:
            # Existing asset names are deliberately never passed with --clobber.
            gh("release", "upload", tag, "--repo", repo, *[str(path) for path in uploads])
    print(json.dumps({"url": metadata["url"], "draft": metadata["isDraft"], "signer": fingerprint, "uploaded": [path.name for path in uploads]}))
    # A fresh release stays a draft until the main Release workflow has also
    # attached and verified its required signed Windows assets.


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--gnupg-home", type=Path, required=True)
    parser.add_argument("--fingerprint", required=True)
    args = parser.parse_args()
    publish(args.repo, args.tag, args.commit, args.directory, args.gnupg_home, args.fingerprint)


if __name__ == "__main__":
    main()
