---
name: openspec-explore
description: Explore product ideas, architecture, requirements, and active OpenSpec changes without implementing code. Use when the user wants a thinking partner, needs to investigate a problem, compare approaches, clarify scope, or reason through a change before or during implementation.
---

# Explore with OpenSpec context

Adopt an exploratory stance, not an implementation workflow. Read files, search code, inspect history, and create or edit OpenSpec artifacts only when the user explicitly asks. Never write application code while this skill is active; if implementation is requested, suggest `$openspec-propose` or `$openspec-apply-change`.

At the start, run `cd mia-docs && openspec list --json` to learn which changes exist. If a named or clearly relevant change exists, read all of its available artifacts before reasoning about it.

Explore naturally:

- Ground claims in the actual codebase and OpenSpec tree.
- Surface assumptions, unknowns, integration points, failure paths, and hidden complexity.
- Compare viable options with their product and architecture consequences.
- Use a compact Mermaid diagram, table, or ASCII sketch when relationships or state transitions are materially clearer visually.
- Ask questions that arise from evidence; do not turn the conversation into a fixed questionnaire.
- Challenge assumptions, including the user's, while keeping closed decisions closed.
- Walk the operating conditions in `mia-docs/openspec/specs/code-practices/operating-conditions.md`: multiple identities, multiple devices, linking timing, unstable connections, full disks, and capability narrowing, revocation, and re-granting.

When an insight becomes durable, offer to capture it, but do not write automatically:

| Insight | Artifact |
| --- | --- |
| New or changed requirement | Delta or main spec for the capability |
| Design decision or invalidated assumption | `design.md` |
| Scope change | `proposal.md` |
| New work | `tasks.md` |

There is no required output. When useful, summarize the crystallized problem, leading approach, unresolved questions, and the next reasonable action. Preserve the user's requested language.
