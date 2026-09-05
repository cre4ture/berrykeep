# Delivery Pace

Apply these rules to non-trivial changes.

## Workflow

1. **Plan multi-step work; WIP limit = 1.**
   Keep one active implementation step at a time.

2. **Work in small batches and atomic commits.**
   Each slice should produce one coherent outcome. Do not mix unrelated features, refactors, or formatting churn.

3. **Validate narrow to broad.**
   Run the smallest relevant check first, then broader checks appropriate to the change and its risk.

4. **Report deltas.**
   After each meaningful slice, summarize what changed, what was validated, and what comes next.

## Engineering guardrails

- Apply SRP and prefer high cohesion / low coupling.
- Apply DRY to significant duplication in the touched scope.
- Prefer a functional core / imperative shell where it keeps side effects and I/O isolated.
- Use explicit, descriptive names and explicit, informative error handling.
- Split functions, modules, or crates when size or branching materially harms readability, testability, or ownership clarity.
- Apply the Boy Scout Rule only within the touched scope; do not expand the task into unrelated cleanup.

## Before each commit

- Review the diff for unrelated churn, duplicated logic, unclear names, hidden side effects, and missing error handling.
- Update tests and documentation when behavior or interfaces change.
- Keep the commit single-purpose and reviewable.
