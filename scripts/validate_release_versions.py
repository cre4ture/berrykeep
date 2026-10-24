#!/usr/bin/env python3
"""Validate the version metadata used to publish an official release."""

from pathlib import Path
import re
import shlex
import sys
import tomllib


def validate_release_versions(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["workspace"]["package"]["version"]
    metadata = sorted(root.glob("apps/*/project.yml")) + sorted(
        root.glob("**/*.xcodeproj/project.pbxproj")
    )
    if not metadata:
        raise ValueError("no Apple version metadata found")
    for path in metadata:
        relative = path.relative_to(root).as_posix()
        values = re.findall(
            r"^[ \t]*MARKETING_VERSION[ \t]*[:=][ \t]*(.*)$",
            path.read_text(),
            re.MULTILINE,
        )
        versions = []
        for value in values:
            if relative.endswith(".pbxproj"):
                value = value.split(";", 1)[0]
            try:
                parts = shlex.split(value, comments=True)
            except ValueError as error:
                raise ValueError(f"{relative}: invalid MARKETING_VERSION {value!r}") from error
            versions.append(parts[0] if len(parts) == 1 else value.strip())
        if not versions or any(value != version for value in versions):
            raise ValueError(
                f"{relative}: expected MARKETING_VERSION {version}, found {versions or 'none'}"
            )
    return version


if __name__ == "__main__":
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent
    try:
        print(validate_release_versions(root))
    except (OSError, KeyError, ValueError) as error:
        print(f"Release version validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
