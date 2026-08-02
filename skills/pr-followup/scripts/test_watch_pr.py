"""Focused tests for the PR watcher check-status adapter."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT_PATH = Path(__file__).with_name("watch_pr.py")
SPEC = importlib.util.spec_from_file_location("watch_pr", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
watch_pr = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(watch_pr)


def check_response(
    contexts: list[dict[str, object]],
    *,
    has_next_page: bool = False,
    end_cursor: str | None = None,
) -> dict[str, object]:
    return {
        "data": {
            "repository": {
                "pullRequest": {
                    "commits": {
                        "nodes": [
                            {
                                "commit": {
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "nodes": contexts,
                                            "pageInfo": {
                                                "hasNextPage": has_next_page,
                                                "endCursor": end_cursor,
                                            },
                                        }
                                    }
                                }
                            }
                        ]
                    }
                }
            }
        }
    }


class CheckContextItemTest(unittest.TestCase):
    def test_check_run_conclusions_are_classified(self) -> None:
        failed = watch_pr.check_context_item(
            {
                "__typename": "CheckRun",
                "name": "Unit tests",
                "status": "COMPLETED",
                "conclusion": "FAILURE",
                "detailsUrl": "https://example.test/failure",
            }
        )
        skipped = watch_pr.check_context_item(
            {
                "__typename": "CheckRun",
                "name": "Optional build",
                "status": "COMPLETED",
                "conclusion": "SKIPPED",
            }
        )
        pending = watch_pr.check_context_item(
            {
                "__typename": "CheckRun",
                "name": "Build",
                "status": "IN_PROGRESS",
            }
        )

        self.assertIsNotNone(failed)
        self.assertIsNotNone(skipped)
        self.assertIsNotNone(pending)
        self.assertEqual("fail", failed["bucket"])
        self.assertEqual("FAILURE", failed["state"])
        self.assertEqual("pass", skipped["bucket"])
        self.assertEqual("pending", pending["bucket"])

    def test_status_context_failure_is_actionable(self) -> None:
        item = watch_pr.check_context_item(
            {
                "__typename": "StatusContext",
                "context": "legacy-ci",
                "state": "ERROR",
                "targetUrl": "https://example.test/error",
            }
        )

        self.assertEqual(
            {
                "bucket": "fail",
                "name": "legacy-ci",
                "state": "ERROR",
                "link": "https://example.test/error",
            },
            item,
        )


class ChecksTest(unittest.TestCase):
    def test_checks_reads_all_graphql_pages_and_keeps_failures(self) -> None:
        responses = [
            check_response(
                [
                    {
                        "__typename": "CheckRun",
                        "name": "first page",
                        "status": "COMPLETED",
                        "conclusion": "SUCCESS",
                    }
                ],
                has_next_page=True,
                end_cursor="next-page",
            ),
            check_response(
                [
                    {
                        "__typename": "CheckRun",
                        "name": "second page",
                        "status": "COMPLETED",
                        "conclusion": "TIMED_OUT",
                    }
                ]
            ),
        ]
        calls: list[tuple[list[str], dict[str, object]]] = []

        def fake_gh_json(
            args: list[str], **kwargs: object
        ) -> dict[str, object]:
            calls.append((args, kwargs))
            return responses.pop(0)

        repo = {"host": "github.example", "owner": "octo", "name": "repo"}
        with patch.object(watch_pr, "gh_json", side_effect=fake_gh_json):
            items = watch_pr.checks(repo, 42)

        self.assertEqual(["pass", "fail"], [item["bucket"] for item in items])
        self.assertEqual(2, len(calls))
        self.assertIn("after=next-page", calls[1][0])
        self.assertTrue(all("allowed" not in kwargs for _, kwargs in calls))


if __name__ == "__main__":
    unittest.main()
