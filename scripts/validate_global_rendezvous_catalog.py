"""Validate the public catalog's schema and cross-entry invariants offline."""

import argparse
import ipaddress
import json
from pathlib import Path
import re
import sys
from urllib.parse import urlsplit

from jsonschema import Draft202012Validator, FormatChecker, SchemaError, ValidationError


ROOT = Path(__file__).resolve().parents[1]


def canonical_origin(url):
    parsed = urlsplit(url)
    host = parsed.hostname
    if not host or parsed.netloc.endswith(":"):
        raise ValueError(f"service URL needs a host and a valid optional port: {url}")
    if parsed.scheme != "https" or parsed.path not in ("", "/"):
        raise ValueError(f"v1 service URL must be an HTTPS origin: {url}")
    port = parsed.port
    if port is not None and not 1 <= port <= 65535:
        raise ValueError(f"invalid service port: {url}")
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        host = host.lower().rstrip(".")
        labels = host.split(".")
        if len(host) > 253 or any(
            not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
            for label in labels
        ):
            raise ValueError(f"invalid service hostname: {url}") from None
    else:
        host = address.compressed
    return host, port or 443


def validate_catalog(catalog, schema):
    Draft202012Validator.check_schema(schema)
    Draft202012Validator(schema, format_checker=FormatChecker()).validate(catalog)
    ids = set()
    origins = set()
    for service in catalog["rendezvous_services"]:
        origin = canonical_origin(service["url"])
        if service["id"] in ids:
            raise ValueError(f"duplicate service ID: {service['id']}")
        if origin in origins:
            raise ValueError(f"duplicate canonical service URL: {service['url']}")
        ids.add(service["id"])
        origins.add(origin)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object key: {key}")
        result[key] = value
    return result


def load_json(path):
    if path.stat().st_size > 64 * 1024:
        raise ValueError(f"JSON file exceeds the 64 KiB limit: {path}")
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", type=Path, default=ROOT / "discovery/global-rendezvous.json")
    parser.add_argument("--schema", type=Path, default=ROOT / "discovery/global-rendezvous.schema.json")
    args = parser.parse_args()
    try:
        catalog = load_json(args.catalog)
        validate_catalog(catalog, load_json(args.schema))
    except (OSError, ValueError, ValidationError, SchemaError) as error:
        parser.error(str(error))
    print(f"Validated {args.catalog}: {len(catalog['rendezvous_services'])} default services")
    return 0


if __name__ == "__main__":
    sys.exit(main())
