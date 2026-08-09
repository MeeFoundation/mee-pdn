---
name: openspec-apply-change
description: Implement tasks from an OpenSpec change and keep its task list current. Use when the user wants to start or continue implementation, execute remaining OpenSpec tasks, or finish a partially applied change.
---

# Apply an OpenSpec change

Work from the repository root and use the OpenSpec CLI in `mia-docs`.

1. Select the change from the user's request or recent context. If only one active change exists, select it. Otherwise run `cd mia-docs && openspec list --json` and ask the user to choose. Announce the selected name and say that another name can be supplied explicitly.
2. Run:

   ```bash
   cd mia-docs && openspec status --change "<name>" --json
   cd mia-docs && openspec instructions apply --change "<name>" --json
   ```

3. Respect the returned state:

   - `blocked`: report the missing artifacts and stop implementation.
   - `all_done`: report completion and suggest `$openspec-archive-change`.
   - otherwise: continue.

4. Read every path in `contextFiles`; do not assume artifact names or locations from the schema.
5. Show the schema, `N/M` progress, remaining tasks, and dynamic instruction.
6. Implement pending tasks in order until all are complete or a real blocker appears:

   - Make the smallest focused code and documentation changes needed for the task.
   - Follow `CLAUDE.md` and the practices under `mia-docs/openspec/specs/code-practices/`.
   - For an affected path, consider the operating conditions: multiple identities, multiple devices, linking timing, dropped connections, full disks, and capability narrowing, revocation, and re-granting. Add a scenario and a test when a condition changes the outcome.
   - Pair every authorized-access assertion with the tightest unauthorized denial.
   - A scenario task is incomplete until its test would fail when the mechanism is removed.
   - Verify the task with the narrowest relevant check, then change its checkbox from `- [ ]` to `- [x]` immediately.
   - For substantial sync, linking, engine, or dependency changes, finish with the focused stress pass required by `flaky-tests.md`.

7. Pause only when the task is unclear, implementation contradicts the design, a command fails without a safe scoped recovery, or the user interrupts. Explain the blocker and the concrete options; do not guess through a design decision.
8. Finish with tasks completed in this run, overall progress, checks run, and either the next task or readiness to archive.

Do not perform state-changing Git operations; `CLAUDE.md` reserves them for a human.
