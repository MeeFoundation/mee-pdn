# Fixed prompts for the deep-review fan-out

These prompts are fixed by the skill. Copy them into `spawn_agent` verbatim, substituting the bracketed placeholders, and do not re-author them for the occasion: a prompt rewritten each run drifts, and the subject it names stops being the subject that was measured. Where a placeholder has no value, use the fallback wording given for it rather than deleting the sentence.

Placeholders: `[DIFF_PATHS]` the scratch dumps; `[FILES]` the changed files, repo-relative; `[SCRATCH]` the scratch directory; `[CONTEXT]` the user's own words; `[LANGUAGE]` the review language; `[MAP]` the map paragraph; `[INTENT_DIR]` the active change's directory; `[GROUP]` a file group for a splittable angle.

Every spawned agent is given `fork_turns: "none"`. It inherits no context, so everything it needs is in the message — that is what keeps the angles independent and blind to each other.

## Common preamble — every agent, every phase

```
Diff: [DIFF_PATHS]. Changed files: [FILES].
Read the sources around the diff, not the diff alone. Cite lines as they stand in the working tree.

Context from the user, to be honoured verbatim:
[CONTEXT]

On running things. You ARE expected to run the one named test that settles a finding — `just test -E 'test(<name>)'`, or the scope-appropriate equivalent in a nested repository — whenever your verdict turns on whether a path is reachable or a test is vacuous. Read-only build queries are equally welcome and cost nothing: `cargo tree -e features`, `cargo metadata`, a grep over `Cargo.toml`. Runs like these are what separate a demonstrated finding from a plausible one. Never assert what a run, a test, or a feature lane WOULD produce without running it: either run it, or say in `evidence` that the claim was derived by reading.

What nobody does: build, lint, or format the tree — no `cargo build` / `check` / `clippy` / `fmt`, no `just check` / `just fix`, no running the suite whole. That belongs to implementation and to CI, and cargo serializes every agent on one artifact lock, which turns this fan-out back into a queue. Never modify a file in the working tree, and run no state-changing git operation.

Write every prose field of your answer in [LANGUAGE]. Identifiers, paths, and type names stay exactly as they are in the code. Never translate these domain terms: capability, connection metadata store / CMS, private metadata store / PMS, claim, lock, race, identity, audience, connection, binder, session, snapshot, ingress, egress.
```

Omit the context block when the request carried none. Omit the language block when the review is in English.

## Map

One agent, before the fan-out. Its answer goes into every search prompt and is persisted so the map survives context compaction.

```
[COMMON PREAMBLE]

Describe what this change does: two or three paragraphs — which mechanism it introduces, which seams it touches, which invariants it bears on. The intent is written down in [INTENT_DIR] — read it. No judgements and no findings. This is the map every other agent receives.

Write the map to [SCRATCH]/map.md before returning it, then return the same text as your final answer. The file is authoritative if the mailbox result is unavailable.
```

Without an active change, replace the intent sentence with: `No change document covers these files.`

## Search — the wrapper every angle carries

```
[COMMON PREAMBLE]

Map of the change:
[MAP]

Your angle is [ANGLE KEY]. [ANGLE PROMPT]

Return at most 5 findings, each with a mechanism you traced through the sources yourself. Do not return a guess with no line of code behind it. Five is a ceiling, not a target: return a finding only if you would advise a human to spend an hour of their working day on it, and return two rather than pad to five. What clears the bar — a wrong answer under some input, data that crosses where it must not, a resource that grows without bound, an invariant a future edit will break unknowingly, a test that would pass with the mechanism removed. What does not, unless it causes one of those — naming, phrasing, a comment, an import, a shape you would have written differently.

For each finding fill in: how it shows up (symptom and exact trigger), what causes it (the mechanism through the code, with lines), who suffers. In `evidence`, say what you actually checked it with — reading the code, running a named test, a probe — and if it was not checked, say so plainly. Then the ways to fix it — one or more, and for each separately: what to do and the size of the edit, the consequences for the product (what changes in the scenarios, what becomes impossible), and the consequences for the architecture (which new constraint it introduces, which invariant appears or hardens, what cannot be done afterwards; if none, write that it introduces none). Then a recommendation: which option and why, or — where the fork needs a human decision — exactly which question is in front of them.

Write your answer to [SCRATCH]/findings-[ANGLE KEY].json before you return it, as one JSON object, and return the same JSON as your final answer:

{"findings": [{"title": str, "file": str, "line": str, "severity": "critical"|"medium"|"structural"|"cleanup", "origin": "new"|"pre-existing", "symptom": str, "cause": str, "victim": str, "evidence": str, "fixes": [{"name": str, "how": str, "product": str, "architecture": str}], "recommendation": str}]}

The file is what survives a run that is cut before you return, so write it first. An angle with nothing to report writes `{"findings": []}` — that is a result, not a failure.
```

Severity is not decoration: it decides how many verifiers a finding gets and what a deadline cuts first. `critical` — data crossing between identities or namespaces, a capability or grant bypassed, a panic or a degradation of the whole node, data lost or corrupted. `medium` — leaks, unbounded growth, degradation under load, rare races. `structural` — an invariant held by convention rather than structure, an API easy to use wrongly, code and spec that have drifted apart. `cleanup` — behaviour unchanged. Include this paragraph in every search prompt.

## The eleven subjects

Fixed. Every one runs every time, one task each. `correctness` and `concurrency` are the two that fan over file groups, and they do so only on a diff too wide for one reader — otherwise each is one task like the rest.

1. **correctness** *(splittable)* — `Correctness. Edge cases, range boundaries, wrap-around, inclusivity, arithmetic, order of operations, counter overflow, unhandled Option / Result paths, early returns that leave state half-built.`
2. **concurrency** *(splittable)* — `Concurrency. A guard held across an await; cancel safety, meaning what happens if the future is dropped at exactly this point; Drop paths and fire-and-forget delivery; races and delivery order; behaviour when a queue is full; what releases a resource when the happy path is not taken.`
3. **access** — `Access. Capability, grant, audience and issuer checks; namespace and identity isolation; tickets and the radius of what they confer; fail-open where fail-closed is required. Invariants by number, from mia-docs/openspec/specs/components/pdn-node/invariants.md — never by name.`
4. **resources** — `Resources. Leaked memory, file descriptors, transactions; unbounded growth; needless fsyncs and round trips; allocation in a hot path; future size. Ask specifically what pins a resource for longer than the thing that acquired it lives.`
5. **invariants** — `Code against the main spec tree, in both directions: mia-docs/openspec/specs/components/**, mia-docs/openspec/architecture/adr/** — where the spec promises what the code does not deliver, and the reverse. When spec text is itself in the diff, an edited requirement that no code implements is as much a finding as code that outran its requirement. Invariants by number, never by name.`
6. **intent** — `The change's own artifacts against the code. They are in [INTENT_DIR]: proposal.md, design.md, tasks.md. Which numbered decisions are implemented, which quietly diverged, which tasks are ticked with no code behind them. Note whether those artifacts are themselves uncommitted — if they are, a divergence can as easily be a spec that drifted as code that did.` Then, always: `Ask the provenance question specifically, because it is the one this angle exists for: is there anything in the code — a constant, a bound, a check, a requirement in the main spec tree — that arrived without a decision, a task, or a delta behind it? Replay the change's delta against the spec tree in your head: if it does not reproduce what the tree now says, the archive has stopped being the record of where the code came from, and that is a finding.` With no active change, replace the first sentences with: `No active change directory covers these files; look for an archived one under mia-docs/openspec/changes/archive/ that does, and say so plainly if there is none rather than inventing an intent.` This angle stays separate from **invariants**: different documents, different questions, and the run that merged them lost the provenance half.
7. **tests** — `Tests. Vacuity, meaning a test that would pass without the change under review — for each test ask what exactly breaks in it if the mechanism is removed. A missing tight denial per mia-docs/openspec/specs/code-practices/access-control-tests.md. Flake risk per mia-docs/openspec/specs/code-practices/flaky-tests.md. Uncovered error paths. Where you suspect vacuity, running one named test is the allowed way to prove it.`
8. **altitude** — `Altitude of the implementation. An invariant held by convention rather than by structure; an API easy to use wrongly (a free-floating Option, representable invalid states, a missing #[must_use]); duplicated logic; a layer leak — pdn-layer knows nothing of data-layer, data-layer knows no capability semantics. Ask whether a call can be constructed in which the new mechanism silently does not apply.`
9. **rules** *(spawn with `reasoning_effort: "low"`)* — `Repository rules. Read CLAUDE.md at the repository root, and pdn-store/CLAUDE.md when the diff touches that repository. Denied lints (unwrap_used, panic, todo, unimplemented, print_*, dbg_macro; as_conversions and expect_used as warnings), max_width = 100, 80 lines per function, cognitive complexity 15. Comments: maximally brief and critical-only, present tense ONLY — no "legacy", "interim", "for now", "used to", "no longer", "until X lands", "future work", no review-finding references, no PR numbers; Dn references are banned in code (they live in the design doc), ADR-XXXX and Invariant N are fine. For anything under mia-docs: one physical line per paragraph, digits with thousands separators, no invented notation or abbreviations, scenario headings at exactly four hashes (a wrong level drops the scenario from validation in silence), the manual sweep the unvalidated main spec tree needs when a change removes or renames something. This is a checklist walked against a grep, not an investigation.`
10. **gaps** — `What the change forgot. Observability and metrics — if the diff adds any, ask who can read them on a running node and what an incident still cannot see; a metric no surface exports is a mechanism with no reader. Rollback. Compatibility with data already written and with peers still on the previous protocol. Error paths, and whether a failure is distinguishable from a success or from unreachability at the place that observes it. Updating the spec to match the new behaviour.`
11. **operating conditions** — `What the change survives: it against mia-docs/openspec/specs/code-practices/operating-conditions.md, condition by condition — several identities on one node (a check that asks "is this our node" where it must ask "is this device one of this identity's"); one device against several (an assumption of a founder device; addressing assembled out of tickets, where a ticket names the device that minted it); a device linking before, during, or after the process; an unstable connection (a refusal indistinguishable from never reaching the peer; a half-completed act a retry cannot converge from; a timeout taken for an answer); a disk that fills (a swallowed write error, a replica reported as converged); a capability granted, narrowed, widened, revoked, and granted again over the same claim. For each condition: does it change the outcome of this change, and if it does, is there a scenario in the spec and a test behind that path. "It does not" is a legitimate answer, but a stated one, not an implied one.`

A splittable angle that was split — the wide-diff case only — carries, appended to its subject: `Your file group is [GROUP NAME]: [GROUP FILES]. Another agent covers the rest of the diff under the same subject — stay in your group.` Its scratch file and task name carry the group name too, so two agents on one subject do not overwrite each other.

## Triage

One agent, spawned with `reasoning_effort: "low"`, on the digest rather than the full findings.

```
[COMMON PREAMBLE]

Map of the change:
[MAP]

[N] candidates came back from [K] independent angles reading the same files, so the same defect arrives several times in different words and never at the same line. Group them by MECHANISM — the thing that is wrong — not by file, line, or wording. Two candidates naming one cause at two call sites are one group; two candidates at one line naming different causes are two groups.

You merge and rank. You do NOT judge: whether a mechanism is real is decided by verification after you, so never drop a candidate for being weak, wrong, or unlikely — only ever as a duplicate of another. A candidate you cannot confidently merge stays on its own. Every index must appear in exactly one group.

For each group return: the member indices, the index of the member whose write-up is the fullest and most accurate (its prose is kept verbatim, so choose it on quality of the mechanism traced), one merged title, and the highest severity among the members.

Candidates:
[DIGEST: one line per candidate — index, angle, severity, file:line, title, symptom]

Write your answer to [SCRATCH]/triage.json and return the same JSON:

{"groups": [{"members": [int], "primary": int, "title": str, "severity": "critical"|"medium"|"structural"|"cleanup"}]}
```

## Verify — one agent per vote

```
[COMMON PREAMBLE]

Try to REFUTE this finding. When in doubt, answer REFUTED.

Title: [TITLE]
Place: [FILE]:[LINE]
Symptom: [SYMPTOM]
Cause: [CAUSE]

Open the sources and check the mechanism line by line. Check whether it is already caught by something above or below in the stack, whether the path is reachable at all, and whether a neighbouring edit in the same diff already changed the behaviour. Running one named test is allowed and expected where it decides the verdict; so are read-only build queries. If the finding is real but its wording overreaches or misattributes, return the sharpened wording in `corrections` — narrowing a finding is doing the job, not softening it. This is pass [K].

Write your answer to [SCRATCH]/verdicts/[MECHANISM SLUG]-[K].json and return the same JSON. Repeat the Title line into the file exactly as you received it — that is the only thing that ties your verdict back to its finding:

{"title": str, "state": "CONFIRMED"|"PLAUSIBLE"|"REFUTED", "reason": str, "lines": str, "corrections": str}
```

`CONFIRMED` — the mechanism holds end to end. `PLAUSIBLE` — it holds, but the trigger depends on conditions the verifier could not check. `REFUTED` — it does not hold. A verifier changes no file in the working tree.

## Gap sweep

One agent, after verification.

```
[COMMON PREAMBLE]

Map:
[MAP]

What was found:
[TITLE, FILE, SEVERITY per surviving finding]

What stayed uncovered? Name: which file or seam of the diff nobody read; which finding went unverified; which subject was asking to be examined and was not; which property of the change cannot be confirmed without a run that was never made. Speak in subjects and seams ("concurrency was not examined", "no one read tables.rs"), never in agents, angles, or votes — this text reaches the reader. Do not invent new findings — name gaps in coverage. Write your answer to [SCRATCH]/gaps.md and return it.
```
