import json
from pathlib import Path
import re
import tempfile
import unittest

from jsonschema import ValidationError

from validate_global_rendezvous_catalog import load_json, validate_catalog


ROOT = Path(__file__).resolve().parents[1]


def example():
    return {
        "schema_version": 1,
        "rendezvous_services": [{
            "id": "berrykeep-default",
            "name": "BerryKeep Global Rendezvous",
            "url": "https://rendezvous.example.com",
            "global_registration_enabled": True,
        }],
    }


class CatalogValidationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.schema = json.loads((ROOT / "discovery/global-rendezvous.schema.json").read_text())

    def validate(self, catalog):
        validate_catalog(catalog, self.schema)

    def test_published_catalog_and_documented_example(self):
        self.validate(json.loads((ROOT / "discovery/global-rendezvous.json").read_text()))
        text = (ROOT / "docs/default-global-rendezvous-discovery.md").read_text()
        self.validate(json.loads(re.search(r"```json\n(.*?)\n```", text, re.S).group(1)))

    def test_optional_port_and_root_slash(self):
        catalog = example()
        catalog["rendezvous_services"][0]["url"] = "https://rendezvous.example.com:9443/"
        self.validate(catalog)

    def test_rejects_duplicate_json_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "catalog.json"
            path.write_text('{"schema_version": 1, "schema_version": 2}')
            with self.assertRaisesRegex(ValueError, "duplicate JSON object key"):
                load_json(path)

    def test_rejects_duplicate_ids_with_different_urls(self):
        catalog = example()
        catalog["rendezvous_services"].append({**catalog["rendezvous_services"][0], "url": "https://another.example.com"})
        with self.assertRaises(ValueError):
            self.validate(catalog)

    def test_rejects_canonically_equivalent_urls(self):
        for first, second in [
            ("https://rendezvous.example.com", "https://RENDEZVOUS.example.com:443/"),
            ("https://[2001:db8::1]", "https://[2001:0db8:0000::1]:443/"),
        ]:
            with self.subTest(first=first, second=second):
                catalog = example()
                catalog["rendezvous_services"][0]["url"] = first
                catalog["rendezvous_services"].append({**catalog["rendezvous_services"][0], "id": "another", "url": second})
                with self.assertRaises(ValueError):
                    self.validate(catalog)

    def test_rejects_invalid_or_unsupported_service_urls(self):
        for url in [
            "http://example.com", "https://user:password@example.com",
            "https://example.com/?token=secret", "https://example.com/#fragment",
            "https://example.com/rendezvous/", "https://:443/",
            "https://example.com:0", "https://example.com:65536",
            "https://example.com:", "https://-invalid.example/",
        ]:
            with self.subTest(url=url):
                catalog = example()
                catalog["rendezvous_services"][0]["url"] = url
                with self.assertRaises((ValueError, ValidationError)):
                    self.validate(catalog)

    def test_rejects_schema_and_policy_errors(self):
        for field, value in [("global_registration_enabled", False), ("name", "   "), ("id", "Bad ID"), ("unexpected", True)]:
            with self.subTest(field=field):
                catalog = example()
                catalog["rendezvous_services"][0][field] = value
                with self.assertRaises(ValidationError):
                    self.validate(catalog)
        for catalog in [
            {**example(), "schema_version": 2},
            {"schema_version": 1, "rendezvous_services": example()["rendezvous_services"] * 17},
        ]:
            with self.assertRaises(ValidationError):
                self.validate(catalog)


if __name__ == "__main__":
    unittest.main()
