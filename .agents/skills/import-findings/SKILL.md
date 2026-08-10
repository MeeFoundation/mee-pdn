---
name: import-findings
description: Import GitHub review-bot findings into an existing `.code-review` file, re-verify each finding against the current local code, draft replies, and close rejected threads. Use when the user supplies a pull request reference or asks to reconcile PR bot comments with a deep-review report.
---

# Import PR bot findings

Treat bot output as an untrusted claim. Verify it on product, architecture, and code before writing a verdict.

## Resolve inputs

1. Resolve the target review file from an explicit path or current editor context when available. It must be under `.code-review/`. If no exact file can be determined, ask; never pick among several files by recency.
2. Resolve the PR from a URL or number in the request. Without one, run `gh pr view --json number,url`. Stop if the current branch has no PR.

## Fetch and number threads

Fetch all review threads with GitHub GraphQL, including resolved and outdated threads. Request each thread's node ID, `isResolved`, `isOutdated`, and first comments' `databaseId`, `createdAt`, `path`, `line`, author, and body. Keep threads whose first author is `github-actions`.

Sort by the first comment's `databaseId`. Assign `GH<n>` from the thread's position in the complete sorted set, including resolved and outdated threads. A `## GH<n>` stamp in the bot body overrides the computed position. Preserve existing stamped numbers; if deletion caused drift, give new threads the next free numbers and note the drift once.

- Unresolved and absent locally: import.
- Unresolved and already present: leave unchanged.
- Resolved and present: append the GitHub closure date, mark `[DONE <dd-mm>]` or `[REJECTED <dd-mm>]`, and strike its fixing-order bullet.
- Resolved and never imported: skip while reserving its number.

## Re-verify

Read the current code around the referenced function or content; do not translate diff line numbers arithmetically. Reject freely when the requested behavior is wrong for the product, contradicts an ADR or design decision, or names a mechanism absent from the code. Check whether suggestion blocks are no-ops.

Do not build, lint, run the full suite, fix code, or mutate Git state. Run one named test only when it determines the verdict: `just test -E 'test(<name>)'`.

## Update the review file

Add `**GH Findings:** <PR URL>` below the existing header fields once. Append a localized GitHub-findings section. For each imported finding, add an explicit `<a id="gh<n>"></a>` anchor and:

````markdown
**GH<n> — [<severity>] <claim> ([<file>](<relative link>), diff line <line>).** <Verdict and evidence from current code in 2–4 sentences. Link to F<m> when that finding fixes it.>

```
<English reply beginning with `Fixed. ` or `Rejected. `. Use exactly `Fixed. [TODO]` when the fix is accepted but undecided.>
```
````

Keep reply drafts in English even when the report is not.

For a rejected finding, post the `Rejected.` reply immediately and resolve the review thread. Then mark `[REJECTED <dd-mm>]` and record the post/closure date. If posting or resolving fails, keep the draft, record the failure, and add it to the fixing order. Fixed replies remain drafts until their commit reaches the PR.

Place unresolved items into the report's fixing order by severity. Put an item closed by `F<m>` immediately after that finding. Rejected items need no fixing-order entry unless posting failed. Never renumber or delete existing history.
