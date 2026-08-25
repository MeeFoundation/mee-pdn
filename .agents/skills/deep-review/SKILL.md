---
name: deep-review
description: Run a rigorous multi-agent review of staged, unstaged, and untracked changes across the root repository and `mia-docs`, then write one prioritized report under `.code-review/`. Use when the user asks for a deep review, independent review angles, adversarial verification, or a comprehensive working-tree assessment.
---

# Deep working-tree review

Multi-agent review of everything uncommitted — staged, unstaged, and untracked — across the two repositories in the tree: the workspace root (`crates/pdn-store`, our iroh-docs variant, included) and `mia-docs` (specs, ADRs, change artifacts). The two are separate git repositories and the second is gitignored by the first, so nothing but this skill's own sweep brings its changes into view. The result is one file in `.code-review/`, arranged so findings can be fixed one at a time, top down.

This skill requires and authorizes subagents: `spawn_agent` for every search angle and every verification vote, `wait_agent` for the mailbox, `list_agents` for the live set, `interrupt_agent` to stop the fan-out at the deadline. The scale is the point — what the agent count buys is depth of findings.

**Review only.** Fix nothing, build nothing, lint nothing, format nothing, never run the suite whole, and run no state-changing git operation in any of the three repositories — per [CLAUDE.md](../../../CLAUDE.md) every `commit` / `add` / `checkout` / `stash` is a human's act. The only writes this skill performs are the scratch directory, the mutation worktree of §5, and one new file under `.code-review/`.

## 0. Parse the request — always parsed, never dropped

Read the request after the skill invocation in this order.

1. **Output language — the first word, optional, matched loosely**, in any spelling and any language of its own (`ru`, `russian`, `русский`, `de`, `deutsch`, `français`, …). It sets the language of the whole review — every finding, every section, the headings, the file — and is then removed from the request. A token naming a path, crate, file, or flag is not a language; **when in doubt, treat it as context** — a review in the wrong language is annoying, a silently swallowed instruction is worse. No language in the first word means English.
2. **`finalize` — a flag, optional, anywhere, matched loosely and in any language** (`finalise`, `финализируй`, `заверши`, `abschließen`, …): stop the fan-out and write the file from what has already come back. It changes when the writing happens, never what is reviewed. See §2b.
3. **Paths, crate names, file names** — narrow what is reviewed.
4. **Everything else is context, and using it is mandatory** — the intent of the change, the author's worries, what to look at first, which decision is closed. It travels **verbatim**, as its own block, into every agent's prompt and into the final file's header — never dropped, never "interpreted into a task".

With no request text: review everything uncommitted, in English, at the scale below.

## 1. Collect the diff yourself — no agents here

Run everything from the repository root. No absolute paths — the skill works in any checkout.

1. `git status --short`, `git -C mia-docs status --short` — both unless the request narrowed the scope. **A clean repository is named as clean under "Not covered"**: "nothing changed there" and "nobody looked" must stay distinguishable.
2. Per repository with changes: `git diff HEAD` (staged and unstaged together). Untracked files never appear in a diff — read them in full and append them to the dump. Exclude orchestration artifacts rather than treating them as product changes: `.claude/worktrees/`, the current scratch directory, and mutation worktrees created by §5. If another generated directory appears, exclude it only when repository rules identify it as generated; record the exclusion under "Not covered".
3. Write the dumps to a scratch directory outside the tracked sources (`<scratch>/wip-diff-<repo>.patch`, untracked content beside), and create `<scratch>/verdicts/`. Agents read the dumps from disk — a large diff passed through prompts eats their context before the work starts.
4. Note the size (files, `+N/-M`) to size the fan-out by. It does not go into the final file.
5. Find the **intent**: an active change under `mia-docs/openspec/changes/*/` whose `proposal.md` / `design.md` / `tasks.md` covers the touched files. Found — its path goes to the "intent" angle; none — that angle runs on the main spec tree alone and the missing half is recorded under "Not covered". Note whether the artifacts are themselves uncommitted: then both sides are in flight, and a divergence can as easily be a spec that drifted as code that did.
6. Invent a `topic` — a short English kebab-case slug (`session-snapshot-egress`, `scoped-grants-binder`).
7. Two timestamps in one call: `date +"%Y-%m-%d_%H-%M"` for the file name, `date +%s` for **T0, the budget clock** (§2a). T0 is taken here, at the start of collecting — everything before the fan-out is on the clock too.
8. **No builds, tests, or lints — not here, not in any agent, not after collection either.** No `just check` / `just fix` / bare `just test`, no `cargo build` / `check` / `clippy` / `fmt`, at any point in the run. Whether the tree compiles and passes is established by the implementer and again by CI on push; a review re-establishing it spends minutes on an answer two other places already report. Exactly two exceptions, both narrow: a named test or a read-only build query that settles one specific finding (§2, "What an agent may and may not run"), and a mutation check in its own worktree (§5).

## 2a. The budget — 60 minutes, and what it does not buy

**60 minutes of wall clock from T0 for the whole run; the fan-out is stopped at T0 + 50 whatever state it is in.** The remaining ten minutes are consolidation and writing, done by you alone — the one part that cannot be cut without losing the run.

A deadline rather than a completion condition: fixing starts as soon as the file exists, so a verdict arriving long after measures a tree that no longer exists. The deadline stops sprawl — nothing more.

**The clock is fixed and the shape is what fits it, on this runtime.** Four concurrency slots, one of them yours, put three tasks in flight; the uncut shape — thirteen search tasks and five votes on a critical mechanism — needs about three hours here, and the hour is the choice that was made. So the shape in §2 is already the cut version, and **the cut lands on the number of votes and nowhere else**: not on the eleven subjects, not on what an angle is asked to read, not on per-angle output, not on the fixes that travel with a finding. Measured, not feared: a run cut along all four at once (nine subjects, three claims per angle, thinned votes, fixes deferred behind verification) returned fourteen mechanisms with zero votes, zero refutations, no way to fix anything, and a false top finding; the run that cut none of them returned nineteen verified mechanisms, killed five, and carried fixes on every one.

**What the thinner verification costs is carried by the verdict word, not by a ledger.** `CONFIRMED` is written only where two independent verdicts agreed, `PLAUSIBLE` covers both a conditional trigger and a mechanism one reader could not fault, `NOT VERIFIED` marks what nobody checked at all. A reader therefore knows what to re-check before acting without the file reporting vote counts, which §3 strips — and "Not covered" stays what it is, a list of subjects and seams, rather than becoming a per-finding tally that every run would trip.

**Priority decides what is verified, because not everything always will be.** Mechanisms are verified critical → medium → structural, so what the deadline leaves unverified is what mattered least. Triage's group order carries no priority.

**Where a run risks missing the deadline, narrow what each agent reads — never remove what it does.** Four concurrency slots, one of them yours, make the search phase a queue three wide: it lasts the number of tasks divided by three, times how long one task takes. Both factors are levers and only one of them is honest — finer file groups and a map that hands out an annotated hunk index shorten every round, while deleting a subject shortens the phase by pretending a file was read. Sizing is the first; the second is a coverage hole the file would then have to declare.

**You are the clock.** After every `wait_agent` return, run `date +%s` and compute `now - T0`. Past 3,000 seconds: `interrupt_agent` every live agent, then finalize per §2b — same turn, without asking, without waiting for the phase in flight. The deadline was chosen in advance precisely so it would not be re-argued at the moment it fires.

**A ceiling, not a quota to spend.** A run whose gap sweep returns at minute 14 is finished at minute 14.

## 2. The fan-out

The shape, sized for three tasks in flight and a 50-minute fan-out: **11 search subjects, one task each, unsplit unless the diff is wide (below)**; **verification votes allotted from what the clock leaves, at most 3 on one mechanism and at most 1 on a medium or structural one**; **at most one mutation check**.

**The clock sizes this, not a task count, and the barriers eat most of it.** In waves of three: the map is one task and about four minutes, eleven angles are four waves and about thirty minutes, triage is about three minutes on the digest, the gap sweep about four — roughly forty-one of the fifty, with the map, triage, and the sweep each holding a wave open for one task because each is a barrier. What is left is about one wave of votes, three of them; two waves on a small diff where the angles come back fast, none at all on a diff that pushed search past four waves. A mutation check spends about one wave of that.

**Allot the votes downward from the top of the severity order, never evenly.** The most severe mechanism takes three, because two agreeing verdicts is what `CONFIRMED` takes and three is what survives one dissent; each further critical takes one; medium and structural take one each while a wave remains; cleanup takes none. Where the budget ends the rest stay `NOT VERIFIED` at the level their severity claims — which is why triage sorts by severity before a single vote is spent.

**The two heaviest subjects split by file group only on a diff too wide for one reader** — beyond roughly 1,500 changed lines or 40 files — and then the beat says the search phase grew by a round. Below that the split buys nothing here: a round is three tasks wide, so a twelfth and thirteenth task cost a whole round of wall clock that verification then does not get.

**The prompts are fixed and live in [`references/angle-prompts.md`](references/angle-prompts.md).** Read that file before spawning anything and copy the prompts verbatim, substituting the placeholders. Do not re-author them per run: a prompt rewritten each time drifts, and the subject it names stops being the subject that was measured.

### Spawning

**Every agent is spawned with `fork_turns: "none"`.** It inherits nothing, so the angles stay independent and blind to one another, and each message must carry everything the agent needs: the dump paths, the changed-file list, the map, the **verbatim user context**, the execution rules, the requirement to read the sources around the diff rather than the diff alone, and its own subject.

Task names are the subject: `search_correctness`, `search_concurrency_group_b`, `verify_2_of_3_<slug>`, `triage`, `gaps`, `mutate_1`. Before passing a task name to `spawn_agent`, normalize every interpolated slug and group name to lowercase ASCII snake case: replace every run outside `[a-z0-9]` with `_`, collapse repeated underscores, and trim leading and trailing underscores. A splittable angle over more than one group carries the normalized group in its name, so two agents on one subject do not collide.

**Every agent writes its answer to its own file under the scratch directory before returning it** — findings to `<scratch>/findings-<angle>.json`, verdicts to `<scratch>/verdicts/<slug>-<k>.json`, triage to `<scratch>/triage.json`, the map and the gap sweep to `<scratch>/`. This is what makes §2b possible: the disk, not your context and not the mailbox, is what a cut run is harvested from.

Keep at most **three subagents** live at once — the Codex orchestrator occupies the fourth collaboration slot. Count with `list_agents`, spawn in rounds, and **preserve every subject**: a subject dropped to fit is a file nobody read, and silence about that reads afterwards as coverage. If the runtime explicitly exposes a different collaboration limit, reserve one slot for the orchestrator and use the rest — and re-derive the vote budget from it rather than keeping the numbers below, which are sized for three. Where a large diff was split and the groups would add more than one round, merge groups down and say in the beat that they were merged — merging, never dropping.

Order of phases: map → search (all angles in one phase, scheduled in Codex-sized rounds) → **triage, a barrier** → verification, pipelined critical first → **the gap sweep, the second barrier**. Only those two phases need the whole set in front of them; verification is not batched — spawn a mechanism's votes as soon as capacity opens, and consume returns as they arrive.

A return that does not parse as the JSON its prompt demanded is re-asked once with `followup_task`, which starts a new turn for an idle agent. Do not use `send_message` for this retry: it delivers text but does not trigger a completed agent. Treat the result as absent if the second answer is malformed too.

### What every angle owes

**A finding leaves the angle whole, fixes included** — the mechanism, then fix options with product and architecture consequences separately, then a recommendation. Writing fixes on candidates that triage will merge and verification will kill is real waste, paid deliberately: fixes staged behind verification sit behind two barriers, and an early finalize then returns mechanisms with no fixes at all — measured, on all fourteen findings of the run that tried it. An unverified finding that says what to do about it is still worth reading; stripped of fixes it is a note to self.

**At most 5 findings per angle, each with a mechanism the agent traced through the sources itself and is prepared to defend.** A guess with no line of code behind it is not returned.

**A second bar, on worth rather than evidence: a finding is returned only if the agent would advise a human to spend an hour of their working day on it.** In 1,000 changed lines there is always one more true-but-inconsequential thing to say, so five is a ceiling, never a target — an angle returning two findings has done its job as well as one returning five. Findings are worked down one at a time by a person reading the code around each; a file of 68 is not five times more useful than a file of 15, it is a week the change does not have. Clears the bar: a wrong answer under some input, data crossing where it must not, unbounded growth, an invariant a future edit will break unknowingly, a test that passes with the mechanism removed. Does not, unless it causes one of those: naming, phrasing, a comment, an import, a shape you would have written differently.

**A claim about what a build, a test run, or a feature lane would produce is either run or marked as reasoning.** "The result is predetermined, so I did not check" already topped one run's findings and was wrong — one `cargo tree` away from being caught.

### Triage — between search and verification

**Candidates are deduplicated and ranked before a single vote is spent on them.** A dozen angles read the same files, so one defect arrives from several of them in different words and never at the same line — a measured run returned 12 findings about one timeout constant at nine line ranges and would have paid 36 verifiers for four mechanisms. Verify-then-merge throws away votes already bought.

Two rules keep triage from becoming a second, sloppier reviewer:

- **It merges and ranks; it does not judge.** A candidate is dropped only as a duplicate, never as "probably wrong"; one it cannot confidently merge stays on its own. Whether a mechanism is real is the verifiers' question.
- **The count of angles that found a mechanism is kept — for the run, not for the reader.** Convergence is the strongest signal a fan-out produces; it breaks ties in the ordering of §3 and reaches the file nowhere.

Triage is one of the two justified barriers: it cannot dedupe candidates that have not arrived. Its price is wall clock — ten minutes on a measured run — paid because merging after voting discards votes already bought. It is clustering, not investigation: spawn it with **`reasoning_effort: "low"`**, on the finding digest, never the full findings. Spawn the `rules` search subject with the same override, and pass it explicitly in both cases — an omitted field inherits the parent's effort, and neither of these two subjects is worth the parent's. Mechanisms leave triage **sorted by severity**, so the deadline cuts the tail rather than the head.

### Verification

A verifier is asked to **refute** a finding, not confirm it, and leans "refuted" when in doubt. Three states: `CONFIRMED` (the mechanism holds end to end), `PLAUSIBLE` (holds, but the trigger depends on conditions it could not check), `REFUTED`. Those are the words one verifier returns; the word that reaches the file is the aggregate below, and the two are not the same thing.

**The ceilings — 3 on one mechanism, 1 on a medium or structural one, 0 on cleanup — are ceilings the clock fills from the top, not entitlements.** The uncut shape is 5 and 3; three slots do not hold it inside 50 minutes. Verification still decides more than any other phase: measured refutation rates were 5 of 13 and 15 of 27 — roughly half of what looks solid to a careful reader dies under attack — which is why the votes that exist go where acting on a wrong finding would cost the most. Cleanup findings are checked by reading and carry no verdict.

**`CONFIRMED` takes two independent verdicts that agree; one verdict never promotes.** A lone confirmation leaves the finding `PLAUSIBLE` — one reader agreeing is a reading, not verification. A split among two or three verdicts resolves to `PLAUSIBLE` as well: disagreement between careful readers is itself an honest verdict, not something an extra vote is invented to break. A refuting majority among two or more verdicts kills the finding outright.

**A lone `REFUTED` is an objection, not a verdict.** A verifier is told to lean "refuted" when in doubt, which is right for an adversary and wrong for a sole judge, so a single refutation never deletes a finding by itself: **read the lines it cites yourself, which costs no slot.** Confirmed from the source — the finding moves to the refuted section with that reason. Not confirmed — spend a second vote if a wave remains; if none does, the finding stays `PLAUSIBLE` and the objection is folded into its own text through `corrections`, so the reader inherits the doubt rather than losing the finding to it.

**Where a mutation check (§5) can settle a mechanism, it outranks votes on it.** One demonstration is worth more than three readings of the same lines, and it is paid out of the same budget: a mutation started early holds a slot through the search phase and so costs about one wave — the three votes that wave would have cast.

A verifier opens the sources and cites lines. **A verifier that sharpens a finding is doing its job**: where the mechanism holds but the wording overreaches, the correction goes in `corrections` and is applied in §3 — the three best findings of one run were all narrowed this way. Changing files in the working tree is **forbidden**.

### What an agent may and may not run

**The expectation first, because the prohibition has been read as covering everything, and it does not:** an agent whose verdict turns on whether a path is reachable or a test vacuous **is expected to run the one named test that settles it** — not to consider running it. Three consecutive runs executed nothing and rested every finding on reading; the one run that executed produced the best-founded finding any of them carry — a panic reproduced twice, with the backtrace, plus a mutation that correctly reattributed it to upstream.

**What nobody does: build, lint, or format the tree, or run the suite whole** — `cargo build` / `check` / `clippy` / `fmt`, `just check`, `just fix`, bare `just test`. Not because they are risky but because they are somebody else's job, done twice already: the implementer before commit, CI on push. And cargo serializes agents on one artifact lock (`Blocking waiting for file lock on artifact directory`): a measured run on a wider runtime had 14 agents issue 27 cargo invocations and one angle queued 10 minutes 22 seconds — the fan-out turned back into a queue, and three slots have even less room to absorb that.

**What stays allowed, and is the exception the ban exists to protect: a named test, and read-only build queries.** `just test -E 'test(<name>)'` (or the nested-repo equivalent); `cargo tree -e features`, `cargo metadata`, a grep over `Cargo.toml` — none takes the artifact lock, so none queues anybody. Rationed by relevance, not count: run when the answer decides the finding, not to have looked. Mutation checks (§5) do build from scratch — in their own worktree with their own target directory, contending with nobody.

The dividing line is what the run produces: the state of the whole tree answers a question this review never asks; evidence about one path lands in the finding it settles.

### Progress

One line to the user per batch of returns, at most one every three minutes, and never between them: `Review: 12 minutes in · search 9/11 · verify 4/16 · 14 mechanisms from 27 findings (2 critical).` No commentary, no restating the scope, no "still waiting" padding — the point of the line is that the user can ignore it.

Elapsed time is computed from T0, not from when the fan-out started: the gap is the diff collection and the intent hunt, real time the user waited.

The verify denominator before triage is findings × votes — an upper bound, written with a trailing `+`; after triage it is the votes the merged groups ask for, exact, and the beat says once how many of them the remaining rounds hold. Two moments matter more than the digits around them: `search N/N` closes the finding set, and the `+` dropping turns the run into a countdown. Before the `+` drops, no estimate of the remaining time is honest, and none is offered. When the verify total lands above what the remaining budget can finish, say so once, in one sentence — votes implied, votes returned, and that the deadline will cut the rest as unverified — then back to plain beats.

**A beat never reports a finding**: findings reach the user once, through the file in `.code-review/`. **Never fabricate a beat** — a line is written only for returns that actually arrived, with numbers actually counted.

## 2b. Finalizing early — the deadline, and the `finalize` flag

**The normal ending of a run, not the exceptional one.** Two paths reach it: the §2a budget expiring, which needs no one's permission, and the user passing `finalize`, the same act asked for earlier. One procedure for both; nothing is re-run.

A run can end here because the verifier bill is unknowable at launch: measured on a wider runtime, 24 merged mechanisms implied 70 votes and 36 of them were cast by minute 48. Three slots cast a fraction of that in the same time, so on this runtime the deadline is the common ending rather than the exceptional one, and the procedure below is the one that runs most often.

1. **Stop the fan-out.** `interrupt_agent` every agent `list_agents` shows live. Nothing else is killed, no file is touched.
2. **Harvest the scratch directory, not your memory.** `<scratch>/findings-*.json` is every angle that returned, complete — findings are not reconstructed from prose, and since fixes travel with mechanisms, a harvested finding is whole whether or not a vote was cast on it. `<scratch>/triage.json` holds the merge; its `members` are indices into the candidate list in angle-return order. Cut before triage returned — the candidates are raw and the merge falls to §3 by hand: the one case where the dedup this skill moved upstream is redone downstream.
3. **Re-attach verdicts to findings by the `Title:` line** each verdict file repeats from the prompt it was given. That line is the only link; match on it.
4. **Consolidate per §3, with one change: a finding's verdict is whatever its returned votes say, under the same rules.** Two of three back and both confirming — `CONFIRMED`, since two agreeing verdicts is what the word takes; a returned refuting majority — the refuted section; a single returned refutation — an objection you check against the lines it cites, exactly as in §2.
5. **A finding with no returned votes is `NOT VERIFIED`, never `CONFIRMED`.** It stays at the level its severity claims — not hidden, and not read as established: verification exists because plausible-looking findings die under it.
6. **A finding keeps its angle's fixes, whatever the deadline did to its votes.** The one case that loses them is an angle that never returned at all — that subject is named under "Not covered" instead.
7. **Write "Not covered" by hand.** An early finalize almost always means the gap sweep never ran (it sits behind the full-set barrier). Say that, and name: the findings that went unverified, the subjects nobody searched — as coverage holes ("concurrency was not examined"), never as agent bookkeeping — and that no gap sweep was performed. Nothing here is left to be inferred from silence.

**The header says it in one line and no more:** "Review cut at the 50-minute budget." — or "Review finalized early on request." The cost lands where it bites — the `NOT VERIFIED` marks and "Not covered" — and the reader neither infers the cut from the shape of the file nor reads a paragraph about it.

**What `finalize` does not do.** It does not lower the bar for a finding — a candidate nobody verified is still reported only if its angle traced a mechanism through the sources. It does not skip §3's deduplication, ordering, or fixing order. And it is not a way to dodge an expensive run: if search itself has not returned, there is nothing to finalize, and the honest answer is to say so rather than write a thin file.

## 3. Consolidation — yours alone, after the fan-out

1. **Drop the `REFUTED`** into their own section, one line each with the refuting reason. Which findings those are was settled in §2 and is not re-decided here: a refuting majority among two or more verdicts, or a lone objection whose cited lines you checked yourself and found to hold. A split is `PLAUSIBLE` and stays, as does an objection the sources did not bear out.
2. **Check the merge rather than redo it.** Catch what triage could not see: two groups the verdicts reveal as one mechanism, one group whose verifiers split because it was two. The same file and line with different mechanisms is not a duplicate — true in triage, true here.
3. **Apply the `corrections`** from the verifiers — the sharpened wording goes into the file.
4. **Strip the machinery from the prose.** Vote counts, angle names and counts, agent counts, phase names — none reaches the file: "found by four angles" is how the run convinced itself; the reader is convinced by the mechanism and the lines. Exactly three traces survive, because they change what the reader does: the verdict word (`CONFIRMED` / `PLAUSIBLE` / `NOT VERIFIED` — trust it, weigh it, or verify it first), the refuted section (do not rediscover these), and the coverage holes under "Not covered". Those holes are named as subjects, which is not bookkeeping: "concurrency was not examined" is a fact about the change, "three concurrency agents returned nothing" is a fact about the run.
5. **Number them `F1..Fn`** in final order — most important first, numbering running through every section. A number is never reused: a closed finding keeps its number in the file.
6. **Sort into levels.** The order of sections is fixed, and the definitions decide what goes where:
   - **Critical — correctness and security.** Data crossing between identities or namespaces; a capability or grant bypassed; a panic or a degradation of the whole node; data lost or corrupted.
   - **Medium — reliability and resources.** Leaks, unbounded growth, degradation under load, rare races. Mechanism confirmed, trigger conditional.
   - **Structural — altitude of the implementation.** An invariant held by convention rather than structure; an API easy to use wrongly; code and spec that have drifted apart.
   - **Cleanup and tests.** Duplication, vacuous tests, missing denials, violations of [CLAUDE.md](../../../CLAUDE.md). Behaviour unchanged.
7. **Within a section**: `CONFIRMED` before `PLAUSIBLE`; ties by convergence (the found-by count triage kept), then by blast radius — how many parties are touched and how irreversibly. Convergence orders; it is never written.
8. **Assemble a fixing order** — a route, not a copy of the findings list: what comes first, what is fixed in one pass together, which fix opens another finding (as a timeout opens a cancellation window), which fork needs a human decision, and which decision.

   **A coupling shapes the route's structure, never only its prose.** Fixes for one sitting share one bullet; a prerequisite stands directly before its dependent — whatever their severities. Severity ranks the findings sections, dependencies rank the route, and the two orderings may disagree; a bullet carrying mixed severities says which, so the ranking stays legible. "Take together with F2" three items away from F2's own bullet has already lost the coupling — the route is worked top-down.

   **Bulleted, never numbered** — a number in this file must name exactly one thing, and a numbered route would stand a second, shifting numbering beside the stable `Fn`.

   **Every `Fn` in this section is a link to its finding** — `[F4](#f4)` — in the bullets and in the closing paragraphs. Each finding heading carries `<a id="f4"></a>` on the line above it, in every section including the refuted one. Inside a finding a reference to another one stays bare — linking there would put a link in nearly every paragraph, and the route would stop being the thing built for jumping.

## 4. The final file

**Read [`references/report-template.md`](references/report-template.md) before writing.** It carries the path, the closed header, the "How to keep this file" block, the finding shape, the section skeletons, and the rules for the text. The whole file is in the language resolved in §0, headings included.

## 5. Mutation checks — where they earn their place

A mutation separates a claim about a mechanism from a demonstration of it: remove the guard a finding calls missing or insufficient, and see whether a test notices. There is no flag — judge per finding, where the answer would change what the review says:

- **A finding says a check is missing or too weak.** The suite staying green with the neighbouring guard removed turns plausible into confirmed; the reverse result kills the finding before it costs anyone a fix.
- **A test is suspected of being vacuous.** Break the mechanism it claims to cover — a test that still passes has proven the accusation; nothing else proves it as cheaply, and without this the accusation is just a reading.
- **A recommendation between two fixes turns on whether today's guard actually holds.** Then the mutation decides the recommendation, not just the confidence.

Not on cleanup findings, not on a mechanism already traced end to end with nothing ambiguous left, not where no test exists to fail — there the finding is "nothing covers this", and a mutation adds no information to it.

**The cost is a worktree, a build from scratch, and one of the three subagent slots for as long as that build runs, so: at most one per run, never two live, started early** — not discovered at minute 45 to have had no time. It is paid out of the vote budget, about one wave of it, which is why it goes to the one finding where a demonstration changes the verdict or the recommendation; a vacuity accusation is the cheapest and most decisive. Four consecutive runs of this review made zero mutations while carrying eleven findings that explicitly asked for one — "remove this and the suite stays green" asserted, never demonstrated, in every case. **A run that makes none says so under "Not covered" and names the findings that wanted one.**

The working tree is never touched, and no branch, commit, or ref is created. The order, in the mutation agent's own message: `git -C <repo> diff HEAD > <scratch>/mutate-<n>.patch` → `git -C <repo> worktree add --detach <scratch>/mut-<n> HEAD` → apply the patch inside the worktree (untracked files are copied in separately, or they are not there) → mutate there → run the one target test with `CARGO_TARGET_DIR` inside the worktree, so it takes no lock the other agents wait on → `git -C <repo> worktree remove --force <scratch>/mut-<n>` when done. The result goes into "How it shows up": "checked by mutation: removing <what> breaks <test>". A mutation considered and skipped needs no note; one that came back the wrong way is reported — that result is the finding's obituary.

## 6. What this skill does not do

- It does not fix code. Fixes are made by a human, one at a time, against this file.
- It runs no state-changing git operation in any of the three repositories. The detached worktree of §5 is the one exception, created outside the tree and removed after.
- It does not rewrite or delete existing files in `.code-review/`. Every run creates a new file with its own timestamp.
- It does not discard the context in the request, and does not substitute its own reading of the task for it.
