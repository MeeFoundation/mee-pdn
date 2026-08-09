---
name: deep-review
description: Run a rigorous multi-agent review of staged, unstaged, and untracked changes across the root repository, `pdn-store`, and `mia-docs`, then write one prioritized report under `.code-review/`. Use when the user asks for a deep review, independent review angles, adversarial verification, or a comprehensive working-tree assessment.
---

# Deep working-tree review

This skill explicitly requires and authorizes Codex subagents for independent review and adversarial verification. Review only: never fix code, build, lint, format, run the whole suite, or perform state-changing Git operations. The only write is a new report under `.code-review/`.

## Parse the request

Interpret the request after `$deep-review` in this order:

1. Optional output language in the first token; default to English.
2. Optional `finalize` flag in any natural-language spelling; use already returned work and stop further fan-out.
3. Paths or crate names that narrow scope.
4. All remaining text as verbatim user context. Include it unchanged in every reviewer prompt and the final header.

## Collect evidence locally

From the root, inspect `git status --short` in the root, `pdn-store`, and `mia-docs`, unless scope excludes one. For every changed repository, capture `git diff HEAD`; read and include every untracked file because Git diff omits them. Place review scratch data in a temporary directory outside tracked sources.

Find an active change under `mia-docs/openspec/changes/` whose proposal, design, or tasks cover the diff. Record whether those artifacts are themselves uncommitted. Create a short kebab-case topic and take one timestamp for `.code-review/YYYY-MM-DD_HH-MM_<topic>.md`.

Do not run checks during collection. A reviewer may run one named test when it settles a specific finding, plus read-only queries such as `cargo metadata` or `cargo tree -e features`. Every unexecuted behavioral claim must say it is reasoning from source.

## Coordinate the review

Use the available collaboration slots in rounds; never exceed the current concurrency limit. Keep agents independent during discovery and share only the raw diff paths, changed-file list, relevant intent artifacts, repository rules, and verbatim user context.

1. Ask one subagent to map the change in 2–3 neutral paragraphs.
2. Search from these 11 angles, distributing them across agents or rounds while preserving every angle:

   - correctness: boundaries, arithmetic, unhandled results, partial state;
   - concurrency: locks across await, cancellation, races, queue pressure;
   - access: identity and namespace isolation, capability/grant/audience/issuer checks;
   - resources: leaks, unbounded growth, I/O and hot-path cost;
   - invariants: code versus main specs and ADRs in both directions;
   - intent: code versus proposal, design decisions, tasks, and delta provenance;
   - tests: vacuity, tight denial, flakes, uncovered errors;
   - altitude: convention-held invariants, easy-to-misuse APIs, layer leaks;
   - rules: `CLAUDE.md`, nested rules, docs formatting, and denied lints;
   - gaps: observability, rollback, compatibility, errors, missing spec updates;
   - operating conditions: every condition in `operating-conditions.md`.

3. Require at most 5 findings per angle and only findings worth at least an hour of human attention. Each finding must include title, file and lines, severity (`critical`, `medium`, `structural`, `cleanup`), origin, exact trigger and symptom, traced cause, victim, evidence, fix options with separate product and architecture consequences, and a recommendation.
4. Consolidate candidates locally by mechanism, not by wording or line. Do not drop a unique candidate merely because it looks weak.
5. Send each distinct critical candidate to all available independent verifier slots; send medium and structural candidates to at least 2 verifiers when capacity permits. Ask verifiers to refute, inspect surrounding source, test reachability, and return `CONFIRMED`, `PLAUSIBLE`, or `REFUTED` with corrections and lines. A refuting majority kills the finding; a split is `PLAUSIBLE`. Cleanup can be checked directly without a vote.
6. Run a final gap sweep after consolidation. If the user requests `finalize`, stop new agent work, harvest completed results, mark unvoted findings `NOT VERIFIED`, and name uncovered angles explicitly.

## Consolidate the report

Drop refuted mechanisms from active findings but preserve them in `Refuted during verification`. Apply verifier corrections. Strip agent counts, angle names, vote counts, and orchestration details; retain only the verdict word, refuted section, and coverage gaps.

Number findings `F1..Fn` once in final order. Use sections in this order:

1. Critical — correctness and security.
2. Medium — reliability and resources.
3. Structural — design altitude and code/spec drift.
4. Cleanup and tests.
5. Refuted during verification.
6. Not covered.

Within a section, put `CONFIRMED` before `PLAUSIBLE`, then sort by blast radius. Build a bulleted fixing route ordered by dependency and coupling rather than merely copying severity order. Link every route reference as `[F<n>](#f<n>)`, and put `<a id="f<n>"></a>` above every finding.

The header contains only:

```markdown
# Review: <topic>

**What changed:** <plain-language summary, at most 100 words, no implementation names>
**Context:** <verbatim user context; omit when absent>
```

For each active finding include file/line, verdict, origin, how it manifests and what was executed, cause with source links, victim, fix options with product and architecture consequences, and recommendation. `Not covered` is mandatory, even when it says no material gaps remain.

Use one physical line per Markdown paragraph, relative clickable links from `.code-review/`, exact identifiers, digits with thousands separators, and repository terminology. `Checked` means executed; use `read` for source inspection. Never reference `F<n>` from tracked code or docs because `.code-review/` is ignored.
