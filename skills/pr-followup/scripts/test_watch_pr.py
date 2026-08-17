"""Focused tests for the PR watcher check-status adapter."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
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


def pr_with_events() -> dict[str, object]:
    return {
        "url": "https://github.example/octo/repo/pull/42",
        "comments": {
            "nodes": [
                {
                    "id": "comment-1",
                    "author": {"login": "reviewer"},
                    "body": "Please rename this.",
                    "createdAt": "2026-08-17T18:00:00Z",
                    "url": "https://github.example/octo/repo/pull/42#issuecomment-1",
                }
            ]
        },
        "reviews": {
            "nodes": [
                {
                    "id": "review-1",
                    "author": {"login": "reviewer"},
                    "body": "One requested change.",
                    "submittedAt": "2026-08-17T18:01:00Z",
                    "state": "CHANGES_REQUESTED",
                }
            ]
        },
    }


def inline_with_event() -> list[dict[str, object]]:
    return [
        {
            "id": "inline-1",
            "user": {"login": "reviewer"},
            "body": "Use a helper here.",
            "created_at": "2026-08-17T18:02:00Z",
            "html_url": "https://github.example/octo/repo/pull/42#discussion_r1",
            "path": "skills/pr-followup/scripts/watch_pr.py",
            "line": 1,
        }
    ]


class EventStateTest(unittest.TestCase):
    repo = {"host": "github.example", "owner": "octo", "name": "repo"}
    number = 42

    def test_first_run_reports_existing_events_then_persists_them(self) -> None:
        identity = watch_pr.event_state_identity(self.repo, self.number)
        with tempfile.TemporaryDirectory() as temporary_directory:
            state_file = Path(temporary_directory) / "watcher.json"
            seen, exists = watch_pr.load_event_state(state_file, identity)

            events = watch_pr.collect_new_events(
                pr_with_events(), inline_with_event(), seen
            )
            watch_pr.save_event_state(state_file, identity, seen)

            self.assertFalse(exists)
            self.assertEqual(3, len(events))

            resumed_seen, resumed_exists = watch_pr.load_event_state(state_file, identity)
            resumed_events = watch_pr.collect_new_events(
                pr_with_events(), inline_with_event(), resumed_seen
            )

            self.assertTrue(resumed_exists)
            self.assertEqual([], resumed_events)

    def test_state_belongs_to_one_pull_request(self) -> None:
        identity = watch_pr.event_state_identity(self.repo, self.number)
        other_identity = watch_pr.event_state_identity(self.repo, self.number + 1)
        with tempfile.TemporaryDirectory() as temporary_directory:
            state_file = Path(temporary_directory) / "watcher.json"
            watch_pr.save_event_state(
                state_file,
                identity,
                watch_pr.empty_seen_events(),
            )

            with self.assertRaises(watch_pr.ConfigError):
                watch_pr.load_event_state(state_file, other_identity)

    def test_invalid_state_is_rejected(self) -> None:
        identity = watch_pr.event_state_identity(self.repo, self.number)
        with tempfile.TemporaryDirectory() as temporary_directory:
            state_file = Path(temporary_directory) / "watcher.json"
            state_file.write_text("{not json", encoding="utf-8")

            with self.assertRaises(watch_pr.ConfigError):
                watch_pr.load_event_state(state_file, identity)

    def test_default_state_file_is_unique_per_pull_request(self) -> None:
        with patch.dict(watch_pr.os.environ, {"XDG_STATE_HOME": "/tmp/state"}):
            current = watch_pr.default_event_state_file(self.repo, self.number)
            other = watch_pr.default_event_state_file(self.repo, self.number + 1)

        self.assertNotEqual(current, other)
        self.assertEqual(current.parent, Path("/tmp/state/ironmesh/pr-followup"))

    def test_main_reports_events_when_creating_a_new_state(self) -> None:
        pr = pr_with_events() | {
            "number": self.number,
            "title": "Persistent event state",
            "state": "OPEN",
            "baseRefName": "main",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
        }
        args = SimpleNamespace(
            pr=None,
            repo=None,
            timeout_seconds=30.0,
            no_notify=True,
            base="main",
            ignore_check=[],
            ignore_existing_failures=False,
            state_file=None,
            interval=5.0,
        )
        identity = {"number": self.number, "url": pr["url"]}

        with tempfile.TemporaryDirectory() as temporary_directory:
            state_file = Path(temporary_directory) / "watcher.json"
            args.state_file = state_file
            with (
                patch.object(watch_pr, "parse_args", return_value=args),
                patch.object(watch_pr.shutil, "which", return_value="gh"),
                patch.object(watch_pr, "identify_pr", return_value=identity),
                patch.object(watch_pr, "snapshot", return_value=pr),
                patch.object(
                    watch_pr,
                    "inline_comments",
                    return_value=inline_with_event(),
                ),
                patch.object(watch_pr, "checks", return_value=[]),
                patch.object(watch_pr, "notify"),
            ):
                self.assertEqual(watch_pr.EXIT_NEW_ACTIVITY, watch_pr.main())

            seen, exists = watch_pr.load_event_state(
                state_file,
                watch_pr.event_state_identity(self.repo, self.number),
            )

        self.assertTrue(exists)
        self.assertEqual({"comment-1"}, seen["comments"])
        self.assertEqual({"review-1"}, seen["reviews"])
        self.assertEqual({"inline-1"}, seen["inline"])


if __name__ == "__main__":
    unittest.main()
