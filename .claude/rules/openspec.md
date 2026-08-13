---
description: OpenSpec conventions and silent-failure gotchas for the mia-docs spec tree
paths:
  - "mia-docs/**"
---

# OpenSpec (mia-docs)

- **`openspec validate --all --strict` before archiving a change.** It is a parser gate for change deltas, not a review: it checks that delta files parse (`## ADDED/MODIFIED/REMOVED/RENAMED Requirements` sections) and that every requirement keeps at least one `#### Scenario:` block. It does not check content — SHALL wording, WHEN/THEN completeness, or truthfulness.
- **Scenario headers must be exactly `#### Scenario:` (four hashes).** A wrong heading level drops the scenario from parsing _silently_ — validation still passes if the requirement retains another scenario. Heading levels are checked by eye, not by the tool.
- **The main spec tree is not validated at all.** `openspec validate --specs` finds nothing in our layout (`openspec/specs/components/**`, `architecture/**` — the tool expects flat `specs/<name>/spec.md`). Everything below is therefore maintained by hand:
  - **Sweep the whole tree when a change removes or renames things.** A change's own deltas are not enough — grep `openspec/specs/**` for names of removed types/tests/mechanisms (precedent: multi-identity-node removed `Binding`/`BindingIndex`, and `pdn-node/namespace-addressing.md` kept naming them until noticed by accident).
  - **Sweep other active changes too.** A change landing first can invalidate assumptions in a sibling change's proposal/design (precedent: subset-rbsr referenced the removed ingest gate, a deleted test, and one-identity phrasing in D4 long after multi-identity-node landed).
  - **Keep `architecture/language/` in sync.** A term used by specs (e.g. swarm) gets a glossary entry there, and the spec links the term's first use to it.
  - **Verify spec scenarios against running code before writing them down.** A plausible formulation once claimed a zero-length write degenerates into deletion; the engine actually rejects it (`AttemptedToInsertEmptyEntry`). Each scenario should correspond to a test or a checked property of the fork.
