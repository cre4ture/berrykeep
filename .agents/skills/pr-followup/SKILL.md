---
name: pr-followup
description: Follow an open BerryKeep pull request after creation or update. Use after opening or pushing to a PR, or when asked to follow it until merge-ready; watch target-branch drift, merge conflicts, review feedback, and CI failures.
---

# PR Follow-up

Read [references/ironmesh-pr-facts.md](references/ironmesh-pr-facts.md) before the first watch. Use [../../../docs/ci-runbook.md](../../../docs/ci-runbook.md) when choosing local CI reproduction and validation commands.

## Workflow

1. Resolve the PR, head branch, and actual target branch.
2. Run the bundled watcher as one blocking command:

   `python3 <pr-followup-skill-dir>/scripts/watch_pr.py [PR] --base <target-branch> --no-notify`

   Let the watcher own polling. While it runs, do not issue parallel GitHub/`gh` status queries or periodic terminal reads. Use `--help` for the full CLI. Common options are `--repo [HOST/]OWNER/REPO`, `--timeout <duration>` (default `20m`; bare numbers mean minutes), `--no-timeout`, repeatable `--ignore-check <glob>`, `--ignore-existing-failures`, and `--state-file <path>`.
3. Treat its exit code as control flow:
   - `1`: investigate the failed CI check, fix and push, then restart the watcher.
   - `2`: address new review feedback; if it is ambiguous, conflicting, or changes product direction, ask the user.
   - `3`: update from the target branch, resolve conflicts, push, then restart the watcher.
   - `0`: no actionable event occurred before the timeout; decide whether to rerun or hand over.
   - `4`: the PR was closed or merged; stop.
   - `64` / `70`: fix configuration or local tooling, then rerun.
   - `130`: the watcher was interrupted; stop and report the interruption.
4. Consider the PR merge-ready only when the latest head is current with the target branch, required checks are complete and green, and no actionable unresolved review findings remain.
5. When stopping, summarize branch freshness, CI state, review state, and the exact blocker if any.

## Review and CI

- Treat GitHub review threads, review decisions, and required checks as the source of truth.
- Ignore stale feedback only when it no longer applies to the current diff; never ignore active change requests or unresolved actionable threads.
- For Windows-specific failures or native CFAPI behavior, use [../windows-ci-access/SKILL.md](../windows-ci-access/SKILL.md).
