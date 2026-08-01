---
name: pr-followup
description: Follow an open Ironmesh pull request after it is created or updated. Use when you just opened a PR, just pushed a commit to a feature branch with an open PR, or when asked to babysit a PR until it is ready to merge by periodically checking GitHub for target-branch drift, merge conflicts, review findings, and CI failures
---

# PR Follow-up

Read [references/ironmesh-pr-facts.md](references/ironmesh-pr-facts.md) before the first poll. Use [../../docs/ci-runbook.md](../../docs/ci-runbook.md) when choosing local CI reproduction and validation commands.

## Workflow

1. Resolve the PR number, head branch, and target branch from the current branch or the caller's explicit PR.
2. Run the bundled blocking watcher instead of manual polling:

   `python3 <pr-follow-up-skill-dir>/scripts/watch_pr.py [PR] --base main`

3. Use the watcher exit codes as the primary control flow:
   - `1`, `2`, `3`: fix the issue and push, then start the watcher again.
   - `0`: timeout expired without actionable events; decide whether to rerun or hand over.
   - `4`: PR is closed or merged; stop.
   - `64` / `70`: resolve tooling or configuration problems, then rerun.
4. If review feedback is concrete and actionable, apply the fix directly. If the feedback is ambiguous, conflicting, or changes product direction, ask the user.
5. Stop only when one of these is true:
   - the PR is merged or closed,
   - the latest head commit is up to date with the target branch, required checks are complete and green, there are no actionable unresolved review findings, and there is nothing else to change,
   - progress requires user input, approval, missing credentials, or an external state change outside the agent's control.
6. When stopping, leave a concise status summary covering branch freshness, CI state, review state, and the exact blocker if any.

## Review And CI Handling

- Prefer GitHub review threads, review decisions, and required checks over local assumptions.
- Ignore stale comments that no longer apply to the current diff, but do not ignore active change requests or unresolved threads.
- If a failure looks Windows-specific or needs native CFAPI behavior, switch to [../windows-ci-access/SKILL.md](../windows-ci-access/SKILL.md).
- If CI is still running for the latest head commit, keep the timer alive; do not stop just because nothing is actionable yet.

<!-- BEGIN pr-follow-up watch-pr integration -->
## Wait for actionable pull-request events

After creating or updating a pull request, use the bundled blocking watcher instead of repeatedly polling by hand. Resolve the directory containing this `SKILL.md`, then run:

```bash
python3 <pr-follow-up-skill-dir>/scripts/watch_pr.py [PR] --base main --no-notify
```

The optional `PR` selector may be a pull-request number, URL, or branch. Without it, the script selects the pull request associated with the current branch. Use `--repo OWNER/REPO` when the working directory does not identify the repository. If the pull request intentionally targets a branch other than `main`, pass that actual target with `--base <branch>`.

The watcher stops after 20 minutes by default when no actionable event occurs. Set another upper bound with `--timeout 45m`, `--timeout 2h`, or a bare number interpreted as minutes. Disable the limit with `--no-timeout` or `--timeout 0`.

By default, every failed check is actionable. Use repeated shell-glob rules such as `--ignore-check 'Deploy *'` for a known permanently non-actionable check. Use `--ignore-existing-failures` to accept only check runs that are already red at startup. A new run, or the same run becoming pending/green and later red again, remains actionable.

The watcher ignores comments and reviews that already exist when it starts, but immediately reports a non-ignored existing failed check, merge conflict, or closed pull request. It exits on the first event that needs attention or when its upper time limit expires:

- exit `0`: no actionable event occurred before the timeout;
- exit `1`: inspect and fix a failed CI check, build, or test;
- exit `2`: read and address the new PR comment, inline comment, or submitted review;
- exit `3`: update from the PR target branch and resolve merge conflicts;
- exit `4`: stop because the pull request was closed or merged;
- exit `64` or `70`: fix configuration or local tooling before retrying;
- exit `130`: the watcher was interrupted manually.

After handling exit `1`, `2`, or `3`, push the fix and start the watcher again. The helper requires Python 3 and an authenticated GitHub CLI (`gh auth status`).
<!-- END pr-follow-up watch-pr integration -->
