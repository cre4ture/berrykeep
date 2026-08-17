#!/usr/bin/env python3
"""Wait for GitHub PR events that require attention.

Requires an authenticated GitHub CLI (`gh`). No Python packages are required.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path
from textwrap import shorten
from typing import Any
from urllib.parse import urlparse

EXIT_CHECK_FAILED = 1
EXIT_NEW_ACTIVITY = 2
EXIT_MERGE_CONFLICT = 3
EXIT_PR_NOT_OPEN = 4
EXIT_TIMEOUT = 0
EXIT_CONFIGURATION = 64
EXIT_RUNTIME = 70

DEFAULT_TIMEOUT_SECONDS = 20 * 60
STATE_VERSION = 1
EVENT_STATE_KINDS = ("comments", "reviews", "inline")

QUERY = r"""
query WatchPr($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      title
      url
      state
      baseRefName
      mergeable
      mergeStateStatus
      comments(last: 100) {
        nodes { id author { login } body createdAt url }
      }
      reviews(last: 100) {
        nodes { id author { login } body submittedAt state }
      }
    }
  }
}
"""

CHECKS_QUERY = r"""
query WatchPrChecks(
  $owner: String!
  $name: String!
  $number: Int!
  $after: String
) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100, after: $after) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    status
                    conclusion
                    detailsUrl
                  }
                  ... on StatusContext {
                    context
                    state
                    targetUrl
                  }
                }
                pageInfo {
                  hasNextPage
                  endCursor
                }
              }
            }
          }
        }
      }
    }
  }
}
"""


class GhError(RuntimeError):
    pass


class ConfigError(GhError):
    pass


class BaseBranchError(ConfigError):
    pass


class EventStateError(ConfigError):
    pass


class WatchTimeout(RuntimeError):
    pass


def remaining_time(deadline: float | None) -> float | None:
    if deadline is None:
        return None
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise WatchTimeout
    return remaining


def gh_json(
    args: list[str],
    *,
    allowed: tuple[int, ...] = (0,),
    empty: Any = None,
    deadline: float | None = None,
) -> Any:
    command = ["gh", *args]
    remaining = remaining_time(deadline)
    command_timeout = 60.0 if remaining is None else min(60.0, remaining)
    try:
        result = subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=command_timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        if deadline is not None and time.monotonic() >= deadline:
            raise WatchTimeout from exc
        raise GhError(f"Konnte {' '.join(command)} nicht ausführen: {exc}") from exc
    except OSError as exc:
        raise GhError(f"Konnte {' '.join(command)} nicht ausführen: {exc}") from exc

    if result.returncode not in allowed:
        detail = result.stderr.strip() or result.stdout.strip() or "unbekannter Fehler"
        raise GhError(
            f"{' '.join(command)} endete mit Status {result.returncode}: {detail}"
        )

    if not result.stdout.strip():
        return empty
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise GhError(f"Ungültige JSON-Ausgabe von gh: {result.stdout[:400]!r}") from exc


def identify_pr(
    pr: str | None,
    repo_arg: str | None,
    deadline: float | None = None,
) -> dict[str, Any]:
    args = ["pr", "view"]
    if pr:
        args.append(pr)
    args += ["--json", "number,url"]
    if repo_arg:
        args += ["--repo", repo_arg]

    data = gh_json(args, empty={}, deadline=deadline)
    if not isinstance(data, dict) or not data.get("number") or not data.get("url"):
        raise GhError("Der Pull Request konnte nicht ermittelt werden.")
    return data


def repo_from_url(url: str) -> dict[str, str]:
    parsed = urlparse(url)
    parts = [part for part in parsed.path.split("/") if part]
    if not parsed.hostname or len(parts) < 4 or parts[2] != "pull":
        raise GhError(f"Unerwartete Pull-Request-URL: {url}")

    owner, name = parts[0], parts[1]
    host = parsed.hostname
    gh_repo = f"{owner}/{name}" if host == "github.com" else f"{host}/{owner}/{name}"
    return {"owner": owner, "name": name, "host": host, "gh_repo": gh_repo}


def event_state_identity(repo: dict[str, str], number: int) -> dict[str, Any]:
    return {
        "host": repo["host"],
        "owner": repo["owner"],
        "name": repo["name"],
        "number": number,
    }


def default_event_state_file(repo: dict[str, str], number: int) -> Path:
    configured_state_home = os.environ.get("XDG_STATE_HOME")
    state_home = (
        Path(configured_state_home)
        if configured_state_home
        else Path.home() / ".local" / "state"
    )
    identity = event_state_identity(repo, number)
    encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
    state_key = hashlib.sha256(encoded).hexdigest()
    return state_home / "ironmesh" / "pr-followup" / f"{state_key}.json"


def empty_seen_events() -> dict[str, set[str]]:
    return {kind: set() for kind in EVENT_STATE_KINDS}


def copy_seen_events(seen: dict[str, set[str]]) -> dict[str, set[str]]:
    return {kind: set(seen[kind]) for kind in EVENT_STATE_KINDS}


def load_event_state(
    state_file: Path,
    identity: dict[str, Any],
) -> tuple[dict[str, set[str]], bool]:
    try:
        contents = state_file.read_text(encoding="utf-8")
    except FileNotFoundError:
        return empty_seen_events(), False
    except OSError as exc:
        raise EventStateError(
            f"Event-State konnte nicht gelesen werden: {state_file}: {exc}"
        ) from exc

    try:
        payload = json.loads(contents)
    except json.JSONDecodeError as exc:
        raise EventStateError(f"Event-State ist kein gültiges JSON: {state_file}") from exc

    if not isinstance(payload, dict) or payload.get("version") != STATE_VERSION:
        raise EventStateError(f"Event-State hat ein unbekanntes Format: {state_file}")
    if payload.get("pull_request") != identity:
        raise EventStateError(
            f"Event-State gehört nicht zu diesem Pull Request: {state_file}"
        )

    stored_seen = payload.get("seen")
    if not isinstance(stored_seen, dict):
        raise EventStateError(f"Event-State enthält keine Event-IDs: {state_file}")

    seen = empty_seen_events()
    for kind in EVENT_STATE_KINDS:
        values = stored_seen.get(kind)
        if not isinstance(values, list) or not all(isinstance(value, str) for value in values):
            raise EventStateError(f"Event-State enthält ungültige {kind}: {state_file}")
        seen[kind] = set(values)
    return seen, True


def save_event_state(
    state_file: Path,
    identity: dict[str, Any],
    seen: dict[str, set[str]],
) -> None:
    payload = {
        "version": STATE_VERSION,
        "pull_request": identity,
        "seen": {kind: sorted(seen[kind]) for kind in EVENT_STATE_KINDS},
    }
    temporary_name: str | None = None
    try:
        state_file.parent.mkdir(parents=True, exist_ok=True)
        descriptor, temporary_name = tempfile.mkstemp(
            dir=state_file.parent,
            prefix=f".{state_file.name}.",
            suffix=".tmp",
        )
        with os.fdopen(descriptor, "w", encoding="utf-8") as temporary_file:
            json.dump(payload, temporary_file, sort_keys=True)
            temporary_file.write("\n")
            temporary_file.flush()
            os.fsync(temporary_file.fileno())
        os.chmod(temporary_name, 0o600)
        os.replace(temporary_name, state_file)
        temporary_name = None
    except OSError as exc:
        raise EventStateError(
            f"Event-State konnte nicht gespeichert werden: {state_file}: {exc}"
        ) from exc
    finally:
        if temporary_name is not None:
            try:
                os.unlink(temporary_name)
            except FileNotFoundError:
                pass


def snapshot(
    repo: dict[str, str],
    number: int,
    deadline: float | None = None,
) -> dict[str, Any]:
    response = gh_json(
        [
            "api",
            "graphql",
            "--hostname",
            repo["host"],
            "-f",
            f"query={QUERY}",
            "-f",
            f"owner={repo['owner']}",
            "-f",
            f"name={repo['name']}",
            "-F",
            f"number={number}",
        ],
        empty={},
        deadline=deadline,
    )
    if not isinstance(response, dict):
        raise GhError("Unerwartete GraphQL-Antwort.")
    if response.get("errors"):
        raise GhError(f"GitHub GraphQL meldet Fehler: {response['errors']}")

    pr = (((response.get("data") or {}).get("repository") or {}).get("pullRequest"))
    if not isinstance(pr, dict):
        raise GhError(f"PR #{number} wurde nicht gefunden.")
    return pr


def connection_nodes(pr: dict[str, Any], field: str) -> list[dict[str, Any]]:
    nodes = ((pr.get(field) or {}).get("nodes") or [])
    if not isinstance(nodes, list):
        raise GhError(f"Unerwartete GraphQL-Daten für {field}.")
    return [item for item in nodes if isinstance(item, dict)]


def inline_comments(
    repo: dict[str, str],
    number: int,
    deadline: float | None = None,
) -> list[dict[str, Any]]:
    endpoint = (
        f"repos/{repo['owner']}/{repo['name']}/pulls/{number}/comments"
        "?per_page=100&sort=created&direction=desc"
    )
    data = gh_json(
        [
            "api",
            "--hostname",
            repo["host"],
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            endpoint,
        ],
        empty=[],
        deadline=deadline,
    )
    if not isinstance(data, list):
        raise GhError("Unerwartete Antwort für Inline-Kommentare.")
    return [item for item in data if isinstance(item, dict)]


def check_context_page(
    repo: dict[str, str],
    number: int,
    after: str | None,
    deadline: float | None = None,
) -> tuple[list[dict[str, Any]], bool, str | None]:
    args = [
        "api",
        "graphql",
        "--hostname",
        repo["host"],
        "-f",
        f"query={CHECKS_QUERY}",
        "-f",
        f"owner={repo['owner']}",
        "-f",
        f"name={repo['name']}",
        "-F",
        f"number={number}",
    ]
    if after is not None:
        args += ["-f", f"after={after}"]

    response = gh_json(
        args,
        empty={},
        deadline=deadline,
    )
    if not isinstance(response, dict):
        raise GhError("Unerwartete GraphQL-Antwort für PR-Checks.")
    if response.get("errors"):
        raise GhError(f"GitHub GraphQL meldet Fehler für PR-Checks: {response['errors']}")

    pr = (((response.get("data") or {}).get("repository") or {}).get("pullRequest"))
    commits = (pr or {}).get("commits") if isinstance(pr, dict) else None
    commit_nodes = (commits or {}).get("nodes") if isinstance(commits, dict) else None
    if not isinstance(commit_nodes, list) or not commit_nodes:
        raise GhError(f"PR #{number} enthält keinen Commit für die Check-Abfrage.")

    commit = (commit_nodes[0] or {}).get("commit")
    rollup = commit.get("statusCheckRollup") if isinstance(commit, dict) else None
    if rollup is None:
        return [], False, None
    contexts = rollup.get("contexts") if isinstance(rollup, dict) else None
    if not isinstance(contexts, dict):
        raise GhError("Unerwartete GraphQL-Daten für PR-Checks.")

    nodes = contexts.get("nodes")
    page_info = contexts.get("pageInfo")
    if not isinstance(nodes, list) or not isinstance(page_info, dict):
        raise GhError("Unvollständige GraphQL-Daten für PR-Checks.")
    has_next_page = bool(page_info.get("hasNextPage"))
    end_cursor = page_info.get("endCursor")
    if has_next_page and not isinstance(end_cursor, str):
        raise GhError("GitHub hat eine weitere Check-Seite ohne Cursor gemeldet.")
    return (
        [item for item in nodes if isinstance(item, dict)],
        has_next_page,
        end_cursor if isinstance(end_cursor, str) else None,
    )


def check_context_item(context: dict[str, Any]) -> dict[str, Any] | None:
    kind = context.get("__typename")
    if kind == "CheckRun":
        name = str(context.get("name") or "unbekannt")
        status = str(context.get("status") or "UNKNOWN")
        conclusion = str(context.get("conclusion") or "UNKNOWN")
        if status != "COMPLETED":
            bucket = "pending"
            state = status
        elif conclusion in {"SUCCESS", "NEUTRAL", "SKIPPED"}:
            bucket = "pass"
            state = conclusion
        else:
            bucket = "fail"
            state = conclusion
        return {
            "bucket": bucket,
            "name": name,
            "state": state,
            "link": str(context.get("detailsUrl") or ""),
        }

    if kind == "StatusContext":
        name = str(context.get("context") or "unbekannt")
        state = str(context.get("state") or "UNKNOWN")
        bucket = (
            "pass"
            if state == "SUCCESS"
            else "pending"
            if state in {"EXPECTED", "PENDING"}
            else "fail"
        )
        return {
            "bucket": bucket,
            "name": name,
            "state": state,
            "link": str(context.get("targetUrl") or ""),
        }

    return None


def checks(
    repo: dict[str, str],
    number: int,
    deadline: float | None = None,
) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    after: str | None = None
    while True:
        contexts, has_next_page, after = check_context_page(
            repo,
            number,
            after,
            deadline,
        )
        items.extend(
            item for context in contexts if (item := check_context_item(context)) is not None
        )
        if not has_next_page:
            return items


def failed_checks(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [item for item in items if item.get("bucket") == "fail"]


def check_identity(item: dict[str, Any]) -> str:
    """Identify one check run well enough to distinguish later workflow runs."""
    name = str(item.get("name") or "unbekannt")
    link = str(item.get("link") or "")
    return f"{name}\n{link}"


def failed_check_ids(items: list[dict[str, Any]]) -> set[str]:
    return {check_identity(item) for item in failed_checks(items)}


def matching_ignore_pattern(name: str, patterns: tuple[str, ...]) -> str | None:
    return next(
        (pattern for pattern in patterns if fnmatch.fnmatchcase(name, pattern)),
        None,
    )


def classify_failed_checks(
    items: list[dict[str, Any]],
    ignore_patterns: tuple[str, ...],
    ignored_existing_ids: set[str],
) -> tuple[
    list[dict[str, Any]],
    list[tuple[dict[str, Any], str]],
    set[str],
]:
    actionable: list[dict[str, Any]] = []
    ignored: list[tuple[dict[str, Any], str]] = []
    current_ids: set[str] = set()

    for item in failed_checks(items):
        identity = check_identity(item)
        current_ids.add(identity)
        name = str(item.get("name") or "unbekannt")
        pattern = matching_ignore_pattern(name, ignore_patterns)
        if pattern is not None:
            ignored.append((item, f"Ignore-Regel: {pattern}"))
        elif identity in ignored_existing_ids:
            ignored.append((item, "bereits beim Start fehlgeschlagen"))
        else:
            actionable.append(item)

    return actionable, ignored, current_ids


_DURATION_RE = re.compile(r"^([0-9]+(?:\.[0-9]+)?)\s*([smh]?)$", re.IGNORECASE)


def parse_duration(value: str) -> float | None:
    normalized = value.strip().lower()
    if normalized in {"off", "none", "disabled"}:
        return None

    match = _DURATION_RE.fullmatch(normalized)
    if match is None:
        raise argparse.ArgumentTypeError(
            "erwartet eine Dauer wie 30s, 20m oder 2h; ohne Suffix werden Minuten verwendet"
        )

    amount = float(match.group(1))
    unit = match.group(2).lower() or "m"
    seconds = amount * {"s": 1, "m": 60, "h": 3600}[unit]
    return None if seconds == 0 else seconds


def format_duration(seconds: float | None) -> str:
    if seconds is None:
        return "deaktiviert"
    if seconds >= 3600 and seconds % 3600 == 0:
        return f"{seconds / 3600:g} h"
    if seconds >= 60 and seconds % 60 == 0:
        return f"{seconds / 60:g} min"
    return f"{seconds:g} s"


def ids(items: list[dict[str, Any]]) -> set[str]:
    return {str(item["id"]) for item in items if item.get("id") is not None}


def review_items(pr: dict[str, Any]) -> list[dict[str, Any]]:
    return [item for item in connection_nodes(pr, "reviews") if item.get("submittedAt")]


def compact(text: Any) -> str:
    value = " ".join(str(text or "").split())
    return shorten(value, width=180, placeholder=" …") if value else "(ohne Text)"


def graph_author(item: dict[str, Any]) -> str:
    return str((item.get("author") or {}).get("login") or "unbekannt")


def collect_new_events(
    pr: dict[str, Any],
    inline: list[dict[str, Any]],
    seen: dict[str, set[str]],
) -> list[dict[str, str]]:
    events: list[dict[str, str]] = []

    comments = connection_nodes(pr, "comments")
    for item in comments:
        item_id = str(item.get("id") or "")
        if item_id and item_id not in seen["comments"]:
            events.append(
                {
                    "time": str(item.get("createdAt") or ""),
                    "kind": "PR-Kommentar",
                    "author": graph_author(item),
                    "text": compact(item.get("body")),
                    "url": str(item.get("url") or pr["url"]),
                }
            )
    seen["comments"].update(ids(comments))

    reviews = review_items(pr)
    for item in reviews:
        item_id = str(item.get("id") or "")
        if item_id and item_id not in seen["reviews"]:
            events.append(
                {
                    "time": str(item.get("submittedAt") or ""),
                    "kind": f"Review {item.get('state') or 'REVIEW'}",
                    "author": graph_author(item),
                    "text": compact(item.get("body")),
                    "url": str(pr["url"]),
                }
            )
    seen["reviews"].update(ids(reviews))

    for item in inline:
        item_id = str(item.get("id") or "")
        if item_id and item_id not in seen["inline"]:
            user = item.get("user") or {}
            path = str(item.get("path") or "unbekannte Datei")
            line = item.get("line") or item.get("original_line")
            location = f"{path}:{line}" if line is not None else path
            events.append(
                {
                    "time": str(item.get("created_at") or ""),
                    "kind": f"Inline-Kommentar ({location})",
                    "author": str(user.get("login") or "unbekannt"),
                    "text": compact(item.get("body")),
                    "url": str(item.get("html_url") or pr["url"]),
                }
            )
    seen["inline"].update(ids(inline))

    return sorted(events, key=lambda event: event["time"])


def ensure_base(pr: dict[str, Any], expected: str) -> None:
    actual = str(pr.get("baseRefName") or "")
    if actual != expected:
        raise BaseBranchError(
            f"PR #{pr['number']} zielt auf '{actual}', nicht auf '{expected}'. "
            "GitHubs Konfliktstatus gilt immer gegenüber dem tatsächlichen PR-Zielbranch."
        )


def notify(title: str, body: str, enabled: bool) -> None:
    print("\a", end="", flush=True)
    if enabled and shutil.which("notify-send"):
        subprocess.run(
            ["notify-send", "--app-name=PR Watcher", title, body],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )


def timestamp() -> str:
    return datetime.now().astimezone().strftime("%Y-%m-%d %H:%M:%S %Z")


def react(
    pr: dict[str, Any],
    current_checks: list[dict[str, Any]],
    events: list[dict[str, str]],
    notify_enabled: bool,
    *,
    ignore_patterns: tuple[str, ...] = (),
    ignored_existing_failure_ids: set[str] | None = None,
    reported_ignored_failures: set[str] | None = None,
) -> int | None:
    if pr.get("state") != "OPEN":
        state = str(pr.get("state") or "UNKNOWN")
        print(f"\n[{timestamp()}] PR #{pr['number']} ist nicht mehr offen: {state}")
        notify(f"GitHub PR #{pr['number']}", f"PR ist jetzt {state}", notify_enabled)
        return EXIT_PR_NOT_OPEN

    conflict = (
        pr.get("mergeable") == "CONFLICTING"
        or pr.get("mergeStateStatus") == "DIRTY"
    )
    if ignored_existing_failure_ids is None:
        ignored_existing_failure_ids = set()
    if reported_ignored_failures is None:
        reported_ignored_failures = set()
    failed, ignored, current_failure_ids = classify_failed_checks(
        current_checks,
        ignore_patterns,
        ignored_existing_failure_ids,
    )

    for item, reason in ignored:
        report_key = f"{check_identity(item)}\n{reason}"
        if report_key in reported_ignored_failures:
            continue
        reported_ignored_failures.add(report_key)
        print(
            f"\n[{timestamp()}] Ignoriere fehlgeschlagenen Check auf "
            f"PR #{pr['number']}:"
        )
        print(f"  - {item.get('name', 'unbekannt')}: {item.get('state', 'UNKNOWN')}")
        print(f"    Grund: {reason}")
        if item.get("link"):
            print(f"    {item['link']}")

    # A failure present at startup is ignored only while that exact check run
    # remains failed. Once it becomes pending/pass/disappears, a later failure
    # of the same check is actionable again.
    ignored_existing_failure_ids.intersection_update(current_failure_ids)

    if conflict:
        print(
            f"\n[{timestamp()}] Merge-Konflikt auf PR #{pr['number']} "
            f"mit '{pr.get('baseRefName')}'.\n  {pr['url']}"
        )

    if failed:
        print(f"\n[{timestamp()}] Fehlgeschlagener Build/Test auf PR #{pr['number']}:")
        for item in failed:
            print(f"  - {item.get('name', 'unbekannt')}: {item.get('state', 'UNKNOWN')}")
            if item.get("link"):
                print(f"    {item['link']}")

    if events:
        print(f"\n[{timestamp()}] Neue Aktivität auf PR #{pr['number']}:")
        for event in events:
            print(f"  - {event['kind']} von @{event['author']}")
            print(f"    {event['text']}")
            print(f"    {event['url']}")

    if conflict:
        notify(
            f"GitHub PR #{pr['number']}",
            f"Merge-Konflikt mit {pr.get('baseRefName')}",
            notify_enabled,
        )
        return EXIT_MERGE_CONFLICT
    if failed:
        notify(
            f"GitHub PR #{pr['number']}",
            f"Check fehlgeschlagen: {failed[0].get('name', 'unbekannt')}",
            notify_enabled,
        )
        return EXIT_CHECK_FAILED
    if events:
        notify(
            f"GitHub PR #{pr['number']}",
            f"Neue Aktivität: {events[0]['kind']}",
            notify_enabled,
        )
        return EXIT_NEW_ACTIVITY
    return None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Wartet auf fehlgeschlagene PR-Checks, neue Kommentare/Reviews "
            "oder Merge-Konflikte."
        )
    )
    parser.add_argument(
        "pr",
        nargs="?",
        help="PR-Nummer, URL oder Branch; Standard: PR des aktuellen Branches",
    )
    parser.add_argument("-R", "--repo", help="[HOST/]OWNER/REPO")
    parser.add_argument(
        "--base",
        default="main",
        help="Erwarteter PR-Zielbranch (Standard: main)",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=15.0,
        help="Polling-Intervall in Sekunden (Standard: 15)",
    )
    parser.add_argument(
        "--ignore-check",
        action="append",
        default=[],
        metavar="GLOB",
        help=(
            "fehlgeschlagenen Check anhand seines Namens ignorieren; "
            "Shell-Globs wie 'Deploy *' sind erlaubt; mehrfach verwendbar"
        ),
    )
    parser.add_argument(
        "--ignore-existing-failures",
        action="store_true",
        help=(
            "beim Start bereits rote Check-Läufe ignorieren; ein späterer neuer "
            "oder erneut fehlschlagender Lauf wird weiterhin gemeldet"
        ),
    )
    parser.add_argument(
        "--state-file",
        type=Path,
        metavar="PATH",
        help=(
            "Datei für den persistenten PR-Event-State; standardmäßig unter "
            "$XDG_STATE_HOME oder ~/.local/state"
        ),
    )
    timeout_group = parser.add_mutually_exclusive_group()
    timeout_group.add_argument(
        "--timeout",
        dest="timeout_seconds",
        type=parse_duration,
        default=DEFAULT_TIMEOUT_SECONDS,
        metavar="DAUER",
        help=(
            "maximale Laufzeit, z. B. 30s, 20m oder 2h; eine Zahl ohne Suffix "
            "bedeutet Minuten; 0/off deaktiviert (Standard: 20m)"
        ),
    )
    timeout_group.add_argument(
        "--no-timeout",
        dest="timeout_seconds",
        action="store_const",
        const=None,
        help="maximale Laufzeit deaktivieren",
    )
    parser.add_argument(
        "--no-notify",
        action="store_true",
        help="Desktop-Benachrichtigung via notify-send deaktivieren",
    )
    args = parser.parse_args()
    if args.interval < 5:
        parser.error("--interval muss mindestens 5 Sekunden betragen")
    return args


def main() -> int:
    args = parse_args()
    notify_enabled = not args.no_notify
    started_at = time.monotonic()
    deadline = (
        None
        if args.timeout_seconds is None
        else started_at + float(args.timeout_seconds)
    )

    if not shutil.which("gh"):
        print("Fehler: GitHub CLI 'gh' wurde nicht gefunden.", file=sys.stderr)
        return EXIT_RUNTIME

    try:
        identity = identify_pr(args.pr, args.repo, deadline)
        repo = repo_from_url(str(identity["url"]))
        number = int(identity["number"])
        pr = snapshot(repo, number, deadline)
        ensure_base(pr, args.base)
        inline = inline_comments(repo, number, deadline)
        current_checks = checks(repo, number, deadline)
        remaining_time(deadline)
    except WatchTimeout:
        print(
            f"[{timestamp()}] Timeout nach {format_duration(args.timeout_seconds)}; "
            "kein relevantes Ereignis festgestellt."
        )
        return EXIT_TIMEOUT
    except GhError as exc:
        print(f"Fehler: {exc}", file=sys.stderr)
        return EXIT_CONFIGURATION

    state_file = args.state_file or default_event_state_file(repo, number)
    state_identity = event_state_identity(repo, number)
    try:
        seen, state_exists = load_event_state(state_file, state_identity)
    except EventStateError as exc:
        print(f"Fehler: {exc}", file=sys.stderr)
        return EXIT_CONFIGURATION
    updated_seen = copy_seen_events(seen)
    events = collect_new_events(pr, inline, updated_seen)

    ignore_patterns = tuple(args.ignore_check)
    ignored_existing_failure_ids = (
        failed_check_ids(current_checks) if args.ignore_existing_failures else set()
    )
    reported_ignored_failures: set[str] = set()

    print(
        f"Überwache {repo['owner']}/{repo['name']} PR #{number}: {pr.get('title', '')}\n"
        f"Zielbranch: {pr.get('baseRefName')} | Intervall: {args.interval:g} s | "
        f"Timeout: {format_duration(args.timeout_seconds)}\n"
        f"{pr['url']}"
    )
    print(
        f"Event-State: {state_file} "
        f"({'geladen' if state_exists else 'neu angelegt'})"
    )
    if ignore_patterns:
        print("Dauerhafte Check-Ignore-Regeln: " + ", ".join(ignore_patterns))
    if args.ignore_existing_failures:
        print("Beim Start bereits fehlgeschlagene Check-Läufe werden ignoriert.")

    # Failures, conflicts, and events not yet recorded in the persistent state
    # require immediate attention. A fresh state intentionally reports the
    # current events once so activity between watcher invocations is not lost.
    exit_code = react(
        pr,
        current_checks,
        events,
        notify_enabled,
        ignore_patterns=ignore_patterns,
        ignored_existing_failure_ids=ignored_existing_failure_ids,
        reported_ignored_failures=reported_ignored_failures,
    )
    try:
        save_event_state(state_file, state_identity, updated_seen)
    except EventStateError as exc:
        print(f"Event-State-Fehler: {exc}", file=sys.stderr)
        return EXIT_CONFIGURATION
    seen = updated_seen
    if exit_code is not None:
        return exit_code

    errors = 0
    while True:
        try:
            remaining = remaining_time(deadline)
            sleep_seconds = args.interval if remaining is None else min(args.interval, remaining)
            time.sleep(sleep_seconds)
            remaining_time(deadline)

            pr = snapshot(repo, number, deadline)
            ensure_base(pr, args.base)
            inline = inline_comments(repo, number, deadline)
            current_checks = checks(repo, number, deadline)
            remaining_time(deadline)
            updated_seen = copy_seen_events(seen)
            events = collect_new_events(pr, inline, updated_seen)
            errors = 0
        except KeyboardInterrupt:
            print("\nÜberwachung beendet.")
            return 130
        except WatchTimeout:
            print(
                f"\n[{timestamp()}] Timeout nach "
                f"{format_duration(args.timeout_seconds)}; "
                "kein relevantes Ereignis festgestellt."
            )
            return EXIT_TIMEOUT
        except BaseBranchError as exc:
            print(f"\nKonfigurationsfehler: {exc}", file=sys.stderr)
            notify(f"GitHub PR #{number}", "PR-Zielbranch wurde geändert", notify_enabled)
            return EXIT_CONFIGURATION
        except EventStateError as exc:
            print(f"\nEvent-State-Fehler: {exc}", file=sys.stderr)
            notify(
                f"GitHub PR #{number}",
                "Event-State konnte nicht gespeichert werden",
                notify_enabled,
            )
            return EXIT_CONFIGURATION
        except GhError as exc:
            errors += 1
            print(f"[{timestamp()}] Temporärer Abruffehler ({errors}): {exc}", file=sys.stderr)
            continue

        exit_code = react(
            pr,
            current_checks,
            events,
            notify_enabled,
            ignore_patterns=ignore_patterns,
            ignored_existing_failure_ids=ignored_existing_failure_ids,
            reported_ignored_failures=reported_ignored_failures,
        )
        try:
            save_event_state(state_file, state_identity, updated_seen)
        except EventStateError as exc:
            print(f"\nEvent-State-Fehler: {exc}", file=sys.stderr)
            notify(
                f"GitHub PR #{number}",
                "Event-State konnte nicht gespeichert werden",
                notify_enabled,
            )
            return EXIT_CONFIGURATION
        seen = updated_seen
        if exit_code is not None:
            return exit_code


if __name__ == "__main__":
    sys.exit(main())
