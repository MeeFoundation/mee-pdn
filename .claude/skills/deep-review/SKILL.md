---
name: deep-review
description: Deep multi-agent review of uncommitted changes → one findings file in .code-review/
argument-hint: "[language] [what to look at / intent of the change]"
---

# Working-tree review

Multi-agent review of everything uncommitted — staged, unstaged, and untracked — across the two repositories in the tree: the workspace root (`crates/pdn-store`, our iroh-docs variant, included) and `mia-docs` (specs, ADRs, change artifacts). The two are separate git repositories and the second is gitignored by the first, so nothing but this command's own sweep brings its changes into view. The result is one file in `.code-review/`, arranged so findings can be fixed one at a time, top down.

**Arguments to this command:** `$ARGUMENTS`

## 0. Arguments — always parsed, never dropped

Read `$ARGUMENTS` in this order.

**1. Output language — the first word, optional, matched loosely**, in any spelling and any language of its own (`ru`, `russian`, `русский`, `de`, `deutsch`, `français`, `español`, …). It sets the language of the whole review — every finding, every section, the headings, the file — and is then removed from the arguments. A token naming a path, crate, file, or flag is not a language; **when in doubt, treat it as context** — a review in the wrong language is annoying, a silently swallowed instruction is worse. No language in the first word means English.

**2. `finalize` — a flag, optional, anywhere, matched loosely and in any language** (`finalise`, `финализируй`, `заверши`, `abschließen`, `finaliser`, …): stop the fan-out and write the file from what has already come back. Removed from the arguments; changes when the writing happens, never what is reviewed. See §2b.

**3. Paths, crate names, file names** — narrow what is reviewed.

**4. Everything else is context, and using it is mandatory** — the intent of the change, the author's worries, what to look at first, which decision is closed. It travels **verbatim**, as its own block, into every agent's prompt and into the final file's header — never dropped, never "interpreted into a task".

With no arguments: review everything uncommitted, in English, at the scale below.

## 1. Collecting the diff — do this yourself, not with agents

Run everything from the repository root. No absolute paths — the command works in any checkout.

1. `git status --short`, `git -C mia-docs status --short` — both unless the arguments narrowed the scope. A clean repository is named as clean under "Not covered": "nothing changed there" and "nobody looked" must stay distinguishable.
2. Per repository with changes: `git diff HEAD` (staged + unstaged). Untracked files never appear in a diff — read them in full and append them to the dump.
3. Write the dumps to the scratchpad (`<scratchpad>/wip-diff-<repo>.patch`, untracked content beside). Agents read them from disk — a large diff passed through prompts eats the context before the work starts.
4. Note the size (files, `+N/-M`) to size the fan-out by. It does not go into the final file.
5. Find the **intent**: an active change under `mia-docs/openspec/changes/*/` whose `proposal.md` / `design.md` / `tasks.md` covers the touched files. Found — its path goes to the "intent" angle; none — that angle runs on the main spec tree alone and the missing half is recorded under "Not covered". Note whether the artifacts are themselves uncommitted: then both sides are in flight, and a divergence can as easily be a spec that drifted as code that did.
6. Invent a `topic` — a short English kebab-case slug (`session-snapshot-egress`, `scoped-grants-binder`).
7. Two timestamps in one call: `date +"%Y-%m-%d_%H-%M"` for the file name, `date +%s` for **T0, the budget clock** (§2a). T0 is taken here, at the start of collecting — not when the `Workflow` call returns: everything before the fan-out is on the clock too.
8. **No builds, tests, or lints — not here, not in any agent.** No `just check` / `just fix` / bare `just test`, no `cargo build` / `check` / `clippy` / `fmt`. Whether the tree compiles and passes is established by the implementer and again by CI on push; a review re-establishing it spends minutes on an answer two other places already report. The single exception is a targeted run producing evidence about one specific finding — §2, "What an agent may and may not run".

**Fix nothing, and run no state-changing git operations.** Per [CLAUDE.md](../../../CLAUDE.md) every `commit` / `add` / `checkout` / `stash` is done by a human, in all three repositories. The only write this command performs is the review file.

## 2a. The budget — 60 minutes, and what it does not buy

**60 minutes of wall clock from T0 for the whole command; the fan-out is stopped at T0 + 50 whatever state it is in.** The remaining ten minutes are consolidation and writing, done by the orchestrator alone — the one part that cannot be cut without losing the run.

A deadline rather than a completion condition: fixing starts as soon as the file exists, so a verdict arriving long after measures a tree that no longer exists. The deadline stops sprawl — nothing more.

**A backstop, never a budget the phases are designed around.** No phase is cut, capped, or merged to fit the clock — not the angles, not per-angle output, not the votes, not the fixes: those levers decide what the review finds. Measured, not feared: a run cut along all four (nine subjects, three claims per angle, 3/2/1 votes, fixes deferred behind verification) returned fourteen mechanisms with zero votes, zero refutations, no way to fix anything, and a false top finding; the uncut run returned nineteen verified mechanisms, killed five, and carried fixes on every one.

**Priority decides what is verified, because not everything always will be.** Mechanisms are verified critical → medium → structural, so what the deadline leaves unverified is what mattered least. Triage's group order carries no priority.

**Where a run misses the deadline, narrow what each agent reads — never remove what it does.** Finer file groups, or a Map phase handing out an annotated hunk index, shorten the slowest angle — which is what the barrier waits for. Deleting subjects does not: the concurrency cap is `min(16, cores − 2)`, a measured run held 14 angles under a cap of 14 with none queued, and the barrier does not clear sooner for the slowest angle having fewer neighbours.

**The clock is enforced from outside the workflow, by the orchestrator.** A workflow script cannot read the time (`Date.now()` throws inside one, so a resumed run replays identically), so the deadline lives in the progress heartbeat (§2), which computes elapsed time from T0 and emits a `FINALIZE-NOW` line past minute 50. That line is acted on the moment it arrives — not weighed, not deferred to the end of the running phase.

**A ceiling, not a quota to spend.** A run whose gap sweep returns at minute 14 is finished at minute 14.

## 2. Orchestration — the `Workflow` call

This command **prescribes** calling `Workflow`; that is the user's explicit consent to multi-agent orchestration. The scale exceeds the default agent-count guideline because what is bought is depth of findings.

The shape: **11 search subjects, the two heaviest split by file group up to a ceiling of 14 search agents** (the machine's concurrency cap — a wider diff gets coarser groups, never a queue); **5 verification votes for a critical mechanism, 3 for a medium or structural one, none for cleanup.**

### Search angles

Each angle is its own agent, blind to the others. The eleven subjects are fixed by this command; their full prompts live in the script (below) — here the roster:

1. **correctness** *(splittable)* — edge cases, boundaries, wrap-around, arithmetic, unhandled `Option` / `Result` paths, early returns leaving state half-built.
2. **concurrency** *(splittable)* — a guard held across an `await`, cancel safety, `Drop` paths, races and delivery order, full queues.
3. **access** — capability / grant / `audience` / `issuer` checks, namespace and identity isolation, ticket radius, fail-open where fail-closed is required; Invariants by number.
4. **resources** — leaks, unbounded growth, needless fsyncs and round trips, hot-path allocation, future size.
5. **invariants** — code against the main spec tree (`components/**`, `architecture/adr/**`), in both directions; an edited requirement no code implements is as much a finding as code that outran its requirement.
6. **intent** — the active change's artifacts against the code: decisions implemented or quietly diverged, tasks ticked with no code behind them, and provenance — behaviour that reached the code or the spec tree with no decision, task, or delta behind it. Kept separate from **invariants**: different documents, different questions, and the run that merged them lost the provenance half.
7. **tests** — vacuity (a test that would pass without the change), missing tight denial per `access-control-tests.md`, flake risk per `flaky-tests.md`, uncovered error paths.
8. **altitude** — invariants held by convention rather than structure, APIs easy to misuse, duplicated logic, layer leaks.
9. **rules** — [CLAUDE.md](../../../CLAUDE.md), plus the docs/OpenSpec rules for anything under `mia-docs`; a checklist walked against a grep, run at low effort.
10. **gaps** — what the change forgot: observability, rollback, compatibility with written data and older peers, error paths, spec updates.
11. **operating conditions** — the change against `code-practices/operating-conditions.md`, condition by condition; "this condition does not change the outcome" is a legitimate answer, but a stated one, never an implied one.

Each angle's prompt carries: the dump paths, the file list, the map from the "Map" phase, the **verbatim context from `$ARGUMENTS`**, the execution rules (§1 step 8 and "What an agent may and may not run"), and the requirement to read the sources around the diff, not the diff alone.

**An angle returns whole findings, fixes included** — the mechanism, then fix options with product and architecture consequences separately, then a recommendation. Writing fixes on candidates that triage will merge and verification will kill is real waste, paid deliberately: fixes staged behind verification sit behind two barriers, and an early finalize then returns mechanisms with no fixes at all — measured, on all fourteen findings of the run that tried it. An unverified finding that says what to do about it is still worth reading; stripped of fixes it is a note to self.

**At most 5 findings per angle, each with a mechanism the agent traced through the sources itself and is prepared to defend.** A guess with no line of code behind it is not returned.

**A second bar, on worth rather than evidence: return a finding only if you would advise a human to spend an hour of their working day on it.** In 1,000 changed lines there is always one more true-but-inconsequential thing to say, so the quota is a ceiling, never a target — an angle returning two findings has done its job as well as one returning five. Findings are worked down one at a time by a person reading the code around each; a file of 68 is not five times more useful than a file of 15, it is a week the change does not have. Clears the bar: a wrong answer under some input, data crossing where it must not, unbounded growth, an invariant a future edit will break unknowingly, a test that passes with the mechanism removed. Does not, unless it causes one of those: naming, phrasing, a comment, an import, a shape you would have written differently.

**A claim about what a build, a test run, or a feature lane would produce is either run or marked as reasoning.** "The result is predetermined, so I did not check" already topped one run's findings and was wrong — one `cargo tree` away from being caught. Execute under the rules below, or state in `evidence` that the claim was derived by reading and never run.

### Triage — between search and verification

**Candidates are deduplicated and ranked before a single vote is spent on them.** A dozen angles read the same files, so one defect arrives from several of them in different words and never at the same line — a measured run returned 12 findings about one timeout constant at nine line ranges and would have paid 36 verifiers for four mechanisms. Verify-then-merge throws away votes already bought.

Two rules keep triage from becoming a second, sloppier reviewer:

- **It merges and ranks; it does not judge.** A candidate is dropped only as a duplicate, never as "probably wrong"; one it cannot confidently merge stays on its own. Whether a mechanism is real is the verifiers' question.
- **The count of angles that found a mechanism is kept — for the run, not for the reader.** Convergence is the strongest signal a fan-out produces; it steers the ordering in §3 and nothing else.

Triage is one of the two justified barriers (the other is the gap sweep): it cannot dedupe candidates that have not arrived. Its price is wall clock — between the first angle returning and the last, no verifier can start; ten minutes on a measured run — paid because merging after voting discards votes already bought, and a run cut before triage has to redo the merge by hand downstream. It is clustering, not investigation: **low effort, on the finding digest, never the full findings.**

### Verification

A verifier is asked to **refute** a finding, not confirm it, and leans "refuted" when in doubt. Three states: `CONFIRMED` (the mechanism holds end to end), `PLAUSIBLE` (holds, but the trigger depends on unchecked conditions), `REFUTED`.

**Votes: 5 critical, 3 medium or structural, 0 cleanup.** This is what verification is for, and it is not where a run economizes: measured refutation rates were 5 of 13 and 15 of 27 — roughly half of what looks solid to a careful reader dies under attack. Cleanup findings are checked by reading and carry no verdict.

**A majority of `REFUTED` kills a finding; a split resolves to `PLAUSIBLE`** — disagreement between careful readers is itself an honest verdict, not something an extra vote is invented to break. The arithmetic stays in the journal; the file gets the word (§3).

**Verification runs in severity order** — critical, then medium, then structural — sorted before the pipeline is built, so the deadline cuts the tail rather than the head.

A verifier opens the sources and cites lines. **A verifier that sharpens a finding is doing its job**: where the mechanism holds but the wording overreaches, the correction goes in `corrections` and is applied in §3 — the three best findings of one run were all narrowed this way. Changing files in the working tree is **forbidden**.

### What an agent may and may not run

**The expectation first, because the prohibition below has been read as covering everything, and it does not:** an agent whose verdict turns on whether a path is reachable or a test vacuous **is expected to run the one named test that settles it** — not to consider running it. Three consecutive runs executed nothing and rested every finding on reading; the one run that executed produced the best-founded finding any of them carry — a panic reproduced twice, with the backtrace, plus a mutation that correctly reattributed it to upstream.

**What nobody does: build, lint, or format the tree, or run the suite whole** — `cargo build` / `check` / `clippy` / `fmt`, `just check`, `just fix`, bare `just test`. Not because they are risky but because they are somebody else's job, done twice already: the implementer before commit, CI on push. And cargo serializes agents on one artifact lock (`Blocking waiting for file lock on artifact directory`): a measured run had 14 agents issue 27 cargo invocations and one angle queued 10 minutes 22 seconds — the fan-out turned back into a queue.

**What stays allowed, and is the exception the ban exists to protect: a named test, and read-only build queries.** `just test -E 'test(<name>)'` (or the nested-repo equivalent); `cargo tree -e features`, `cargo metadata`, a grep over `Cargo.toml` — none takes the artifact lock, so none queues anybody. Rationed by relevance, not count: run when the answer decides the finding, not to have looked. Mutation checks (§5) do build from scratch — in their own worktree with their own `target`, contending with nobody.

The dividing line is what the run produces: the state of the whole tree answers a question this review never asks; evidence about one path lands in the finding it settles.

### Script

**The script is a file beside this one, not a listing here: [`deep-review.js`](deep-review.js).** Run it by path and do not copy it into the call:

```
Workflow({ scriptPath: '.claude/skills/deep-review/deep-review.js', args: { … } })
```

**`args` is an actual JSON value, never a JSON-encoded string** — a stringified object reaches the script as a string and the first `.join` throws before any agent starts: a run that cost eight milliseconds and produced nothing. The script normalizes defensively; the call should still be right.

| Field       | Type                       | What it carries                                                            |
| ----------- | -------------------------- | -------------------------------------------------------------------------- |
| `language`  | string, optional           | The review's language, from §0. Absent means English.                      |
| `context`   | string, optional           | The user's own words from §0, verbatim. Absent when the request carried none. |
| `diffPaths` | string[]                   | The scratchpad dumps from §1 step 3. Agents read them from disk.            |
| `files`     | string[]                   | Changed files, repo-relative.                                              |
| `intentDir` | string, optional           | The active change's artifacts from §1 step 5. Absent when there is none, and the "intent" angle is told so rather than left to guess. |
| `groups`    | `[{name, files}]`, optional | File groups the two splittable angles fan over. Absent means one group covering everything. |

The subjects and their prompts live in the script because they are fixed by this command, and re-authoring them per run is how they drift. The caller decides the file grouping — and only that.

**The prose here is the policy; the file is the only implementation.** No fragment of the script is reproduced in this document — the markdown copy was both at once until it was extracted, which is exactly how the two came apart. Where a rule here and the file disagree, the file is what ran.

Three properties worth knowing without opening it: **exactly two barriers** — triage and the gap sweep each need the whole set in front of them, and verification between them is a pipeline, so one mechanism's votes are cast while another's are still out; **mechanisms enter it sorted by severity**, so the deadline cuts the tail; **the splittable angles fan over `args.groups`**, merging groups down (and logging the merge) rather than dropping any when the fan would breach the 14-agent ceiling — a dropped group is a file nobody read, and silence about that reads afterwards as coverage.

Both extracted artifacts are checkable without a run, and are checked when either is edited: `node --check .claude/skills/deep-review/deep-review.js` and `python3 -m py_compile .claude/skills/deep-review/deep-review-progress.py`.

The workflow runs in the background. Do not poll it by hand — arm the heartbeat below and wait for the notification. On an empty or unexpected result, read `journal.jsonl` in the run's transcript directory before drawing conclusions.

### The progress heartbeat

The run takes up to fifty minutes and says nothing while it works, so arm a `Monitor` immediately after the `Workflow` call returns — every stdout line becomes a notification. **It is also the run's only clock** (§2a): it carries the budget as well as the counters, and it is what ends the run at T0 + 50.

Elapsed time is computed **from T0** (§1 step 7), not from when the monitor was armed — the gap is the diff collection and the intent hunt, real time the user waited, which a monitor-relative clock would silently forgive.

The counters come from `journal.jsonl` (path in the `Workflow` result). **Count the phases apart, and never take `"type":"started"` for a denominator** — that line is written when an agent gets a concurrency slot, not when the script asks for it, so everything queued behind the cap is missing from it: one journal showed 29 started verifiers on a run holding 176. Count `"type":"result"` lines instead, and tell the phases apart by the result's shape: `findings` — a search angle; `groups` — triage; `state` — a verifier; a bare string — the map or the gap sweep. The search denominator is the number of angles, known up front. **The verify denominator is derived, not observed**: before triage it is findings × votes (5 critical, 3 medium/structural, 0 cleanup) — an upper bound wearing a trailing `+`; after triage it is the merged groups, exact.

Both the counting and the deadline live in [`deep-review-progress.py`](deep-review-progress.py) beside this file, which prints one status line per call and a `FINALIZE-NOW` line once the deadline has passed. The `Monitor` is the loop around it and nothing more:

```bash
D=<transcriptDir>                 # from the Workflow result
NL=<number of search angles>      # 9 whole-change angles + the splittable fan: 11 with one file group, 13 with two or more
T0=<epoch seconds>                # from §1 step 7, `date +%s`
END=$((SECONDS + 3900))
while [ $SECONDS -lt $END ]; do
  .claude/skills/deep-review/deep-review-progress.py "$D" "$NL" "$T0" 3000
  [ -f "$D/../done" ] && break
  sleep 180
done
```

`3000` is the fan-out deadline in seconds — T0 + 50 minutes, §2a. Passed in rather than baked into the script, so the budget has exactly one home: this document.

Arm it with `persistent: false` and a `timeout_ms` covering the budget; stop it with `TaskStop` the moment the workflow notification lands — a heartbeat outliving its run reports a finished number forever.

**A beat is one line, exactly the numbers it carried:** `Review: 12 minutes in · search 11/13 · verify 14/60 · 18 mechanisms from 31 findings (2 critical).` No commentary, no restating the scope, no "still waiting" padding — the point of the line is that the user can ignore it. A heartbeat never reports a finding: findings reach the user once, through the file in `.code-review/`.

**A `FINALIZE-NOW` line is acted on, not reported.** `TaskStop` the workflow and the heartbeat, finalize per §2b — same turn, without asking, without waiting for the phase in flight. The deadline was chosen in advance precisely so it would not be re-argued at the moment it fires; the one line written to the user is the finalize, not a request to confirm one.

Two moments in a beat matter more than the digits around them: `search N/N` closes the finding set — nothing further will be discovered; the `+` dropping makes the verify denominator final — from there the run is a countdown. Before the `+` drops, no estimate of the remaining time is honest, and none is offered.

**When the verify total lands above what the remaining budget can finish, say so once, in one sentence** — votes implied, votes returned, and that the deadline will cut with the rest marked unverified — then back to plain beats. Critical-first order makes this information, not an alarm: what the deadline drops is the tail.

Each beat costs a model turn — a three-minute cadence over a 50-minute budget is about seventeen of them. That is the default; two minutes only if the user asks to watch closely, and say which was chosen.

**Never fabricate a heartbeat.** A line is written only for a notification that actually fired, with numbers actually read from the journal. A progress line invented between notifications is a false report of work.

## 2b. Finalizing early — the deadline, and the `finalize` flag

**The normal ending of a run, not the exceptional one.** Two paths reach it: the §2a budget expiring — announced by `FINALIZE-NOW`, needing no one's permission — and the user passing `finalize`, the same act asked for earlier. One procedure for both; nothing is re-run.

A run can end here because the verifier bill is unknowable at launch. Measured: 58 findings implying 133 verification agents, and 68 implying 176 (both pre-triage, paying for duplicates); with triage, 70 votes over 24 mechanisms, 36 of them done at minute 48 — the run the 50-minute fan-out is sized on.

The flag is honoured whether the run is still going or was stopped earlier.

1. **Stop the fan-out.** `TaskStop` the workflow's task id and the heartbeat. Nothing else is killed, no file is touched.
2. **Harvest `journal.jsonl` in the run's transcript directory.** Every `"type":"result"` line carries that agent's complete return value — findings are not reconstructed from prose, and since fixes travel with mechanisms, a harvested finding is complete whether or not a vote was cast on it. Shapes as in the heartbeat section; triage's `members` are indices into the candidate list in angle-return order. Cut before triage returned — the candidates are raw and the merge falls to §3 by hand: the one case where the dedup this command moved upstream is redone downstream.
3. **Re-attach verdicts to findings.** The journal holds `agentId` and the verdict, not the link; it lives in the verifier's own transcript, `agent-<agentId>.jsonl`, whose prompt carries the `Title:` line it was given. Match on that title.
4. **Consolidate per §3, with one change: a finding's verdict is whatever its returned votes say.** Two of three back, both confirming — `CONFIRMED`; a returned refuting majority — the refuted section.
5. **A finding with no returned votes is `NOT VERIFIED`, never `CONFIRMED`.** It stays in the level its severity claims — not hidden, and not read as established: verification exists because plausible-looking findings die under it.
6. **A finding keeps its angle's fixes, whatever the deadline did to its votes.** The one case that loses them is an angle that never returned at all — that subject is named under "Not covered" instead.
7. **Write "Not covered" by hand.** An early finalize almost always means the gap sweep never ran (it sits behind the full-set barrier). Say that, and name: the findings that went unverified, the subjects nobody searched — as coverage holes ("concurrency was not examined"), never as agent bookkeeping — and that no gap sweep was performed. Nothing here is left to be inferred from silence.

**The header says it in one line and no more:** "Review cut at the 50-minute budget." — or "Review finalized early on request." The cost lands where it bites — the `NOT VERIFIED` marks and "Not covered" — and the reader neither infers the cut from the shape of the file nor reads a paragraph about it.

**What `finalize` does not do.** It does not lower the bar for a finding — a candidate nobody verified is still reported only if its angle traced a mechanism through the sources. It does not skip §3's deduplication, ordering, or fixing order. And it is not a way to dodge an expensive run: if search itself has not returned, there is nothing to finalize, and the honest answer is to say so rather than write a thin file.

## 3. Consolidation — do this yourself, after the workflow

1. **Drop the `REFUTED`** into their own section, one line each with the refuting reason. A refuting majority kills; a split is `PLAUSIBLE` and stays.
2. **Check the merge rather than redo it.** Catch what triage could not see: two groups the verdicts reveal as one mechanism, one group whose verifiers split because it was two. The same file and line with different mechanisms is not a duplicate — true in triage, true here.
3. **Apply the `corrections`** from the verifiers — the sharpened wording goes into the file.
4. **Strip the machinery from the prose.** Vote counts, angle names and counts, agent counts, phase names — none reaches the file: "найдено четырьмя углами" is how the run convinced itself; the reader is convinced by the mechanism and the lines. Exactly three traces survive, because they change what the reader does: the verdict word (`CONFIRMED` / `PLAUSIBLE` / `NOT VERIFIED` — trust it, weigh it, or verify it first), the refuted section (do not rediscover these), and the coverage holes under "Not covered".
5. **Number them `F1..Fn`** in final order — most important first, numbering running through every section. A number is never reused: a closed finding keeps its number in the file.
6. **Sort into levels.** The order of sections is fixed:
   - **Critical — correctness and security.** Data crossing between identities or namespaces; a capability or grant bypassed; a panic or a degradation of the whole node; data lost or corrupted.
   - **Medium — reliability and resources.** Leaks, unbounded growth, degradation under load, rare races. Mechanism confirmed, trigger conditional.
   - **Structural — altitude of the implementation.** An invariant held by convention rather than structure; an API easy to use wrongly; code and spec that have drifted apart.
   - **Cleanup and tests.** Duplication, vacuous tests, missing denials, violations of [CLAUDE.md](../../../CLAUDE.md). Behaviour unchanged.
7. **Within a section**: `CONFIRMED` before `PLAUSIBLE`; ties by convergence (the kept found-by count), then by blast radius — how many parties are touched and how irreversibly. Convergence orders; it is never written.
8. **Assemble a fixing order** — a route, not a copy of the findings list: what comes first, what is fixed in one pass together, which fix opens another finding (as a timeout opens a cancellation window), which fork needs a human decision, and which decision.

   **A coupling shapes the route's structure, never only its prose.** Fixes for one sitting share one bullet; a prerequisite stands directly before its dependent — whatever their severities. Severity ranks the findings sections, dependencies rank the route, and the two orderings may disagree; a bullet carrying mixed severities says which, so the ranking stays legible. "Take together with F2" three items away from F2's own bullet has already lost the coupling — the route is worked top-down.

   **Bulleted, never numbered** — a number in this file must name exactly one thing, and a numbered route would stand a second, shifting numbering beside the stable `Fn`.

   **Every `Fn` in this section is a link to its finding** — `[F4](#f4)` — in the bullets and in the closing paragraphs. Each finding heading carries `<a id="f4"></a>` on the line above it, in every section including the refuted one. Inside a finding a reference to another one stays bare — linking there would put a link in nearly every paragraph, and the route would stop being the thing built for jumping.

   **The anchor is written, never derived from the heading.** Headings get edited — `[DONE <dd-mm>]` on closing, titles sharpened after the fact, review languages whose headings do not transliterate — and a written `f4` survives all of it.

## 4. The final file

Path: `.code-review/<YYYY-MM-DD>_<HH-MM>_<topic>.md` — from `date +"%Y-%m-%d_%H-%M"`. Year first so the files sort by time; no spaces, so the path survives a shell without quoting.

The template below is in English. When the review language is something else, the whole file is in that language, headings included — one document in one language.

**The header is closed: those fields and nothing else.** No diffstat, no search inventory, no build state, no self-assessment — whatever wants a place there belongs in a finding if it is one, and under "Not covered" if it is a limit of the review. A reader opens this file for the findings and reaches the first one within a screen.

````markdown
# Review: <topic>

<The heading is that and nothing more — no repository, no "working tree", no date, no parenthetical of any kind. The file name already carries the date and the topic.>

**What changed:** <at most 100 words on what the change does and why, in plain prose. No file, module, type, trait, or function names — a reader who has not opened the diff has to follow it. This is the only summary of the change in the file; the findings carry the detail.>

**Context:** <the request that asked for the review, verbatim, and nothing else — no gloss, no restatement. Omit the line entirely when the request carried none.>

<One line, only when the run did not reach its own end: "Review cut at the 50-minute budget." or "Review finalized early on request.">


**How to keep this file.** A closed finding is marked `[DONE <dd-mm>]` in its heading, the account of the fix is appended to it under a "What was done" block, and its line in the fixing order is struck through. The text of the finding is not deleted — it stays as the thing the fix is checked against. Finding numbers live only as long as this file: `.code-review/` is not under git, so nothing that is — no code, no documentation — may reference an `F<n>`, which would resolve to nothing for anyone without this file. A comment or a spec line that wants to cite a finding inlines its substance instead. Between one fix and the next the touched tests are stressed briefly and narrowly — a set cut down to what that fix reaches, minutes rather than tens of minutes — and what came back is recorded in that fix's "What was done" block; the full pass is its own item in the fixing order, never something a single fix claims in passing.

## Fixing order

- **[F<n>](#f<n>) — <what>.** <One or two lines: why first, what is risked by deferring it.>
- **[F<n>](#f<n>) + [F<m>](#f<m>) — <what>.** <Why in one pass.>
- ...

- **<A full stress pass — its own item, last, and sized in tens of minutes. Include it whenever the findings imply fixes deep enough for the flaky-test practice to bite — sync, the actor, engine wiring, storage, a dependency bump. Say what the set covers, why the short runs between fixes do not stand in for it, and that its failures only mean anything on a machine doing nothing else: a competing build or a laptop going to sleep turns a network-dependent integration test into noise.>**


<A paragraph on couplings, if any: which fixes open other findings, which forks need a human decision before code is written.>

---

## Critical — correctness and security

<a id="f1"></a>
### F1 — <symptom → consequence, in one line>
**File:** `<path:line>` · **CONFIRMED** <or **PLAUSIBLE**, or **NOT VERIFIED** when the run ended before a single verifier reached it — the word alone, never a vote tally, an angle name, or a count of who found it> · *new in this change* | *pre-existing, but in scope because <reason>*

**How it shows up:** <the exact trigger: which input, which state, what the affected party observes. What it was checked with — a run, a mutation, a probe; if it was never reproduced, say so plainly.>

**What causes it:** <the mechanism through the code with line references: what exactly permits this and why the existing checks do not catch it.>

**Who suffers:** <which party and what it loses: the node, the issuer, the audience, an embedder, an operator, a future refactor.>

**How to fix it:**

- **Option A — <name>.** <What to do, size of the edit.>
  - *For the product:* <what changes in the scenarios; what becomes impossible.>
  - *For the architecture:* <which constraint it introduces, which invariant appears or hardens, what cannot be done afterwards.>
- **Option B — <name>.** <The same.>
  - *For the product:* …
  - *For the architecture:* …

**Recommendation:** <which option and why. If the fork has to be resolved by a human, say exactly which question is in front of them.>

<a id="f2"></a>
### F2 — …

## Medium — reliability and resources

## Structural — altitude of the implementation

## Cleanup and tests

## Refuted during verification

<a id="f<n>"></a>
### F<n> — REFUTED — <title>
<One or two lines: what was supposed and what refuted it.>

## Not covered

<From the gap sweep: which seam nobody read, which findings went unverified, which subject was not searched, what cannot be confirmed without a run that never happened. Everything the header no longer carries lands here: a repository looked at and found clean, an intent that could not be measured for want of a change document, whether the gap sweep itself ran. Coverage holes are named as subjects — "concurrency was not examined" — never as agent or vote bookkeeping. This section is mandatory — if there are no gaps, say that in one line.>
````

There is sometimes only one way to fix something — then there is one option, but the "what to do / for the product / for the architecture" structure stays. An option introducing no architectural constraint says so: "introduces no constraint".

## 5. Mutation checks — where they earn their place

A mutation separates a claim about a mechanism from a demonstration of it: remove the guard a finding calls missing or insufficient, and see whether a test notices. There is no flag — judge per finding, where the answer would change what the review says:

- **A finding says a check is missing or too weak.** The suite staying green with the neighbouring guard removed turns plausible into confirmed; the reverse result kills the finding before it costs anyone a fix.
- **A test is suspected of being vacuous.** Break the mechanism it claims to cover — a test that still passes has proven the accusation; nothing else proves it as cheaply, and without this the accusation is just a reading.
- **A recommendation between two fixes turns on whether today's guard actually holds.** Then the mutation decides the recommendation, not just the confidence.

Not on cleanup findings, not on a mechanism already traced end to end with nothing ambiguous left, not where no test exists to fail — there the finding is "nothing covers this", and a mutation adds no information to it.

**The cost is a worktree and a build from scratch, so: up to three per run, started early, in parallel with the search phase** — not discovered at minute 45 to have had no time. Spend them where they move a verdict or a recommendation; a vacuity accusation is the cheapest and most decisive. Four consecutive runs made zero mutations while carrying eleven findings that explicitly asked for one — "remove this and the suite stays green" asserted, never demonstrated, in every case. A run that makes none says so under "Not covered" and names the findings that wanted one.

The working tree is never touched. The order: `git -C <repo> diff HEAD > <scratchpad>/mutate.patch` → an agent with `isolation: 'worktree'` → apply the patch inside the worktree (untracked files are copied in separately, or they are not there) → mutate → run the target test. The result goes into "How it shows up": "checked by mutation: removing <what> breaks <test>". A mutation considered and skipped needs no note; one that came back the wrong way is reported — that result is the finding's obituary.

## 6. Rules for the text


- **The review is written in the language resolved in §0**, English by default. Never translated, whatever the language: capability, connection metadata store / CMS, private metadata store / PMS, claim, lock, race, identity, audience, connection, binder, session, snapshot, ingress, egress. Identifiers, paths, and type names in backticks, exactly as in the code. A mechanism is called by the word the code gives it — `reclaim` for `reclaim_abandoned_sessions` — never by a metaphor coined for the finding; an operation the code does not name is described in plain words rather than given a new one.

- **A paragraph is one physical line** — the documentation rule from [CLAUDE.md](../../../CLAUDE.md). Lists and headings take one line per item.
- **Links are clickable and relative to `.code-review/`**: `[fs.rs:827](../crates/pdn-store/src/store/fs.rs#L827)`, `[core.md](../mia-docs/openspec/specs/components/mee-pdn/pdn-node/core.md)`.
- **Numbers are digits with thousands separators** (`10,000,000 records`).
- **No invented abbreviations or notation.** Invariants and ADRs by number; `Dn` only when it is said whose decisions those are.
- **"Checked" means executed.** What was read with eyes is called read.
- **Line numbers are as they stand in the working tree** at the time of the review, and are not adjusted afterwards.
- **No votes, no angles, no agents in the file** — §3's strip rule. The verdict word, the refuted section, and "Not covered" are the only traces of the machinery.
- **None of these rules is written into the file.** They govern how it is produced; the reader is not told how the sausage is made.
- **Prototypes**: the pre-pivot one is `v3-single-device`, the rebuild after it is `v3-multi-device`, the current generation is `v4-non-keri`; never a bare number.

## 7. What this command does not do

- It does not fix code. Fixes are made by a human, one at a time, against this file.
- It runs no state-changing git operations, in any of the three repositories.
- It does not rewrite or delete existing files in `.code-review/`. Every run creates a new file with its own timestamp.
- It does not discard the context in the arguments, and does not substitute its own reading of the task for it.
