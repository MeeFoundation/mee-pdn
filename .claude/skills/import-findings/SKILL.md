---
name: import-findings
description: Import a PR bot's review findings into the open review file under GHn prefixes, re-verified against local code
argument-hint: "[pr url or number]"
---

# Import PR bot findings

Pull the review bot's inline findings from a GitHub PR, re-verify every one against the current local code, and record them in the review file under `GH<n>` prefixes — each with a verdict and a ready-to-post reply. The bot is a low-quality instrument: nothing it says is trusted — on product, on architecture, or on the code — until re-checked here, and "we will not fix this" is a first-class outcome.

**Arguments to this command:** `$ARGUMENTS`

## 0. The target file — a gate, not a guess

The target is the review file currently open in the editor, and it must be a file under `.code-review/`. No such file open — stop and ask which review file to use: with several in the directory, guessing writes findings into the wrong one.

## 1. Arguments

`$ARGUMENTS` is a PR reference: a full URL or a bare number. With no arguments, the current branch's PR (`gh pr view --json number,url`). If the branch has no PR, say so and stop.

## 2. Fetching — all threads, then filter locally

Fetch **all** review threads of the PR, not only the unresolved ones — §3's numbering depends on the full list. REST does not expose thread resolution; use GraphQL:

```
gh api graphql -f query='query { repository(owner: "<owner>", name: "<repo>") { pullRequest(number: <n>) { reviewThreads(first: 100) { nodes { isResolved isOutdated comments(first: 5) { nodes { databaseId createdAt path line author { login } body } } } } } } }'
```

Keep the threads whose first comment is authored by the review bot (`github-actions`). `gh` is already authenticated in this environment; GraphQL rides the same token.

## 3. Numbering — GHn is the thread's ordinal, forever

Sort the bot's threads by the first comment's `databaseId` — GitHub's global monotone counter, so ascending id **is** creation order, with same-second `createdAt` ties resolved for free. **`GH<n>` is the thread's position in that full sorted list**, resolved and outdated threads included. The number is a pure function of the PR's state: it never shifts when threads resolve, a thread the bot adds later gets the next number, and re-running the command re-derives the same numbers — idempotency with no local bookkeeping. An entry whose `GH<n>` is already in the file is an update target, never a duplicate.

The review workflow (`.github/workflows/ocr-review.yml`) stamps the same number visibly into each comment — a `## GH<n>` heading at the top of the body, derived by the same ordering. A stamp wins over the computed position: a disagreement means a comment was deleted (not resolved) and the positions below it shifted — keep the stamped numbers for existing entries, give fresh threads the next free numbers, and note the drift in the section preamble.

Thread state → what happens in the file:

- **Unresolved, not yet in the file** — import it (§4, §5).
- **Unresolved, already in the file** — leave the entry as it is; the human is working through it.
- **Resolved, already in the file** — closure on GitHub is the completion signal: mark `[DONE <dd-mm>]` (`[REJECTED <dd-mm>]` when the verdict was a rejection) if the human has not already, append "закрыт на GH <dd-mm>" (in the file's language), strike its fixing-order bullet. The entry's text stays.
- **Resolved, never imported** — skip it. Its number stays reserved by position and appears nowhere.

## 4. Re-verification — the distrust is the method

Every imported finding is re-checked against the **current** working tree before a word is written, by reading the code it points at — the bot reviewed a diff that may be several fixes old, and its line numbers are diff-relative: resolve them to the current code by function and content, never by line arithmetic.

Judge each finding on three planes, and reject freely on any of them:

- **Product** — does the behaviour it wants even belong to this system? A suggestion that weakens a test (an `eventually` around a negative assertion: green instantly, before the violation had a chance to happen) is a rejection, not a style choice.
- **Architecture** — does it contradict a documented decision (a coarse lock chosen on purpose, a boundary held by design)? Cite the document that decides it.
- **Code** — is the mechanism it claims even there? Check the attached suggestion block for no-ops — the bot has proposed byte-for-byte the existing code with a comment added; that fact goes into the reply.

**No builds, no lints, no full test runs.** The one execution allowed is a single named test when the verdict turns on it (`just test -E 'test(<name>)'`). Fix nothing and run no state-changing git operations — the only write is the review file. Replies are **drafts** the human posts, with one exception: the rejected path of §5.

## 5. Writing the entries

The first import adds one line to the review file's header, right below its last header field:

```markdown
**GH Findings:** <the PR's URL>
```

Added once; later imports leave it alone. It is the file's only pointer to which PR the `GH` numbers index into.

Entries go into a `## Замечания <bot> в PR #<n> (GHn)` section at the end of the review file — created if missing, in the file's language, with a one-line preamble stating when the bot ran and which diff it saw. Each entry:

```markdown
**GH<n> — [<severity as the bot tagged it>] <one-line essence of the claim> ([<file>](<relative link>), диффовая строка <line>).** <Verdict and the re-verification in two-to-four sentences: what was checked in the current code, what holds, what does not. If it closes through an existing finding: «закрывается через [F<m>](#f<m>)». If it is fixed separately from any F: say what the fix is. If rejected: why, on which plane.>

​```
<The reply to post on the thread, in English, starting with `Fixed. ` or `Rejected. `. Decided to fix but the variant is not chosen yet — `Fixed. [TODO]` and nothing else. The fence is there so the text copies out raw.>
​```
```

The reply drafts are always English, whatever language the file is in.

**Rejected entries are posted and closed by this command, in the same run.** Post the reply onto the thread (REST: `POST /repos/{owner}/{repo}/pulls/{n}/comments/{comment_id}/replies` — authored by the `gh` user, which is the human's account) and resolve the thread (GraphQL `resolveReviewThread`, keyed by the thread's node id). Then finish the bookkeeping immediately: `[REJECTED <dd-mm>]` after the `GH<n>`, «ответ запощен, тред закрыт <dd-mm>» appended to the entry, no fixing-order bullet. The bot files new remarks on every push, so answered threads leave the board the moment the answer exists; an unposted rejection would keep its noise on the agenda. If a post or resolve fails, the entry stays a draft with the failure noted, and that one thread falls to the human.

**Fixed entries stay drafts.** A `Fixed.` reply posted before the fix's commit reaches the PR would point at nothing; the human posts it with the push and marks `[DONE <dd-mm>]`. Entries closing through an `F` finding likewise wait for that finding's fix.

## 6. The fixing order

Priorities place the work; the numbering never moves. One bullet per imported entry in the file's «Порядок исправления», positioned by severity against what is already there:

- Closes through an existing finding — a bullet right after that finding's own: `- Ответить на [GH<n>](#gh<n>) — закрывается правкой [F<m>](#f<m>).`
- Fixed separately from any F — its own bullet, placed by priority: `- **[GH<n>](#gh<n>) — <what>.**` The bullet covers posting the `Fixed.` reply once the fix's commit reaches the PR.
- Rejected — no bullet: §5 already posted and resolved it. Only a failed post appears: `- Ответить на [GH<n>](#gh<n>) — постинг не прошёл: <причина>.`

Give each entry an anchor (`<a id="gh<n>"></a>` on the line above it) so the order can link to it, same as the `F` findings.

## 7. What this command does and does not post

- It posts **rejected** replies and resolves their threads (§5) — nothing else: no `Fixed.` replies ahead of their commits, no labels, no thread deletions, no edits of the bot's comments.
- It does not fix code, and runs no state-changing git operations.
- It does not renumber or rewrite existing `GH` entries — updates are appends (the resolved mark of §3, the posted mark of §5), never edits of the human's verdict text.
- It does not import resolved threads, and does not delete entries whose threads resolved — the file keeps its history.
