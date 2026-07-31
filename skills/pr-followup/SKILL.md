---
name: pr-followup
description: Follow an open Ironmesh pull request after it is created or updated. Use when you just opened a PR, just pushed a commit to a feature branch with an open PR, or when asked to babysit a PR until it is ready to merge by periodically checking GitHub for target-branch drift, merge conflicts, review findings, and CI failures
---

# PR Follow-up

Read [references/ironmesh-pr-facts.md](references/ironmesh-pr-facts.md) before the first poll. Use [../../docs/ci-runbook.md](../../docs/ci-runbook.md) when choosing local CI reproduction and validation commands.

## Workflow

1. Resolve the PR number, head branch, and target branch from the current branch or the caller's explicit PR.
2. Check regularly, but at most once per 20-minute interval. Start or re-arm a 20-minute sleep timer after any relevant PR activity, then do one poll when it expires. Do not do any investigation during that waiting time to save token costs.
3. After sleeping, treat remote GitHub state for the latest pushed head commit as the source of truth. On each poll, inspect and address.
6. If review feedback is concrete and actionable, apply the fix directly. If the feedback is ambiguous, conflicting, or changes product direction, ask the user.
7. After every push, assume a fresh cycle starts. Reset the sleep timer from that push time like starting from step 2 again.
8. Stop only when one of these is true:
   - the PR is merged or closed,
   - the latest head commit is up to date with the target branch, required checks are complete and green, there are no actionable unresolved review findings, and there is nothing else to change,
   - progress requires user input, approval, missing credentials, or an external state change outside the agent's control.
9. When stopping, leave a concise status summary covering branch freshness, CI state, review state, and the exact blocker if any.

## Review And CI Handling

- Prefer GitHub review threads, review decisions, and required checks over local assumptions.
- Ignore stale comments that no longer apply to the current diff, but do not ignore active change requests or unresolved threads.
- If a failure looks Windows-specific or needs native CFAPI behavior, switch to [../windows-ci-access/SKILL.md](../windows-ci-access/SKILL.md).
- If CI is still running for the latest head commit, keep the timer alive; do not stop just because nothing is actionable yet.
