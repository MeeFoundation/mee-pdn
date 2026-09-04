# The review file — template and the rules for its text

Read this before writing the file. Path: `.code-review/<YYYY-MM-DD>_<HH-MM>_<topic>.md`, from `date +"%Y-%m-%d_%H-%M"`. Year first so the files sort by time; no spaces, so the path survives a shell without quoting.

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

<From the gap sweep: which seam nobody read, which findings went unverified, which subject was not searched, what cannot be confirmed without a run that never happened. Everything the header no longer carries lands here: a repository looked at and found clean, an intent that could not be measured for want of a change document, whether the gap sweep itself ran, and whether any mutation check was made — naming the findings that asked for one. Coverage holes are named as subjects — "concurrency was not examined" — never as agent or vote bookkeeping. This section is mandatory — if there are no gaps, say that in one line.>
````

There is sometimes only one way to fix something — then there is one option, but the "what to do / for the product / for the architecture" structure stays. An option introducing no architectural constraint says so: "introduces no constraint".

## Rules for the text

- **The review is written in the language resolved from the request**, English by default, the whole file including headings. Never translated, whatever the language: capability, connection metadata store / CMS, private metadata store / PMS, claim, lock, race, identity, audience, connection, binder, session, snapshot, ingress, egress. Identifiers, paths, and type names in backticks, exactly as in the code. A mechanism is called by the word the code gives it — `reclaim` for `reclaim_abandoned_sessions` — never by a metaphor coined for the finding; an operation the code does not name is described in plain words rather than given a new one.
- **A paragraph is one physical line** — the documentation rule from [CLAUDE.md](../../../../CLAUDE.md). Lists and headings take one line per item.
- **Links are clickable and relative to `.code-review/`**: `[fs.rs:827](../crates/pdn-store/src/store/fs.rs#L827)`, `[core.md](../mia-docs/openspec/specs/components/pdn-node/core.md)`.
- **Numbers are digits with thousands separators** (`10,000,000 records`).
- **No invented abbreviations or notation.** Invariants and ADRs by number; `Dn` only when it is said whose decisions those are.
- **"Checked" means executed.** What was read with eyes is called read.
- **Line numbers are as they stand in the working tree** at the time of the review, and are not adjusted afterwards.
- **No votes, no angles, no agents in the file.** The verdict word, the refuted section, and "Not covered" are the only traces of the machinery.
- **A number names exactly one thing.** `F1..Fn` run through every section in final order; a number is never reused, and a closed finding keeps its number.
- **Anchors are written, never derived from a heading.** Headings get edited — `[DONE <dd-mm>]` on closing, titles sharpened, languages whose headings do not transliterate — and a written `f4` survives all of it.
- **None of these rules is written into the file.** They govern how it is produced; the reader is not told how the sausage is made.
- **Prototypes**: the pre-pivot one is `v3-single-device`, the rebuild after it is `v3-multi-device`, the current generation is `v4-non-keri`; never a bare number.
