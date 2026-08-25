export const meta = {
  name: 'deep-review',
  description: 'Review of uncommitted changes: search angles, triage, adversarial verification, gap sweep',
  whenToUse: 'Invoked by the /deep-review command. Not run on its own — it expects the args that command assembles.',
  phases: [
    { title: 'Map', detail: 'what the change does' },
    { title: 'Search', detail: 'independent angles over the diff, findings with fixes' },
    { title: 'Triage', detail: 'merge duplicate mechanisms, rank' },
    { title: 'Verify', detail: 'an attempt to refute each mechanism, critical first' },
    { title: 'Gaps', detail: 'what stayed uncovered' },
  ],
}

// ---------------------------------------------------------------------------
// Input. `args` is documented in SKILL.md beside this file, "Script".
//
//   language   string | undefined  the review's language, prose fields only
//   context    string | undefined  the user's own words, honoured verbatim
//   diffPaths  string[]            scratchpad dumps the agents read from disk
//   files      string[]            changed files, repo-relative
//   intentDir  string | undefined  the active change's artifacts, or absent
//   groups     [{name, files}]     file groups the splittable angles fan over
//
// A JSON-encoded string here is the one input mistake that costs a whole run:
// it reaches the script as a string and the first `.join` throws before any
// agent starts. Normalize rather than crash.
// ---------------------------------------------------------------------------
const A = typeof args === 'string' ? JSON.parse(args) : (args || {})

const CEILING = 14 // search agents: the concurrency cap, min(16, cores - 2)
const RANK = { critical: 0, medium: 1, structural: 2, cleanup: 3 }
const VOTES = { critical: 5, medium: 3, structural: 3, cleanup: 0 }

// The nine angles that read the change whole, one agent each.
const WHOLE = [
  { key: 'access', prompt: `Access. Capability, grant, audience and issuer checks; namespace and identity isolation; tickets and the radius of what they confer; fail-open where fail-closed is required. Invariants by number, from mia-docs/openspec/specs/components/pdn-node/invariants.md — never by name.` },
  { key: 'resources', prompt: `Resources. Leaked memory, file descriptors, transactions; unbounded growth; needless fsyncs and round trips; allocation in a hot path; future size. Ask specifically what pins a resource for longer than the thing that acquired it lives.` },
  { key: 'invariants', prompt: `Code against the main spec tree, in both directions: mia-docs/openspec/specs/components/**, mia-docs/openspec/architecture/adr/** — where the spec promises what the code does not deliver, and the reverse. When spec text is itself in the diff, an edited requirement that no code implements is as much a finding as code that outran its requirement. Invariants by number, never by name.` },
  { key: 'intent', prompt: `The change's own artifacts against the code.${A.intentDir ? ` They are in ${A.intentDir}: proposal.md, design.md, tasks.md. Which numbered decisions are implemented, which quietly diverged, which tasks are ticked with no code behind them. Note whether those artifacts are themselves uncommitted — if they are, a divergence can as easily be a spec that drifted as code that did.` : ` No active change directory covers these files; look for an archived one under mia-docs/openspec/changes/archive/ that does, and say so plainly if there is none rather than inventing an intent.`}\n\nAsk the provenance question specifically, because it is the one this angle exists for and the one a merged angle loses: is there anything in the code — a constant, a bound, a check, a requirement in the main spec tree — that arrived without a decision, a task, or a delta behind it? Replay the change's delta against the spec tree in your head: if it does not reproduce what the tree now says, the archive has stopped being the record of where the code came from, and that is a finding.` },
  { key: 'tests', prompt: `Tests. Vacuity, meaning a test that would pass without the change under review — for each test ask what exactly breaks in it if the mechanism is removed. A missing tight denial per mia-docs/openspec/specs/code-practices/access-control-tests.md. Flake risk per code-practices/flaky-tests.md. Uncovered error paths. Where you suspect vacuity, running one named test is the allowed way to prove it.` },
  { key: 'altitude', prompt: `Altitude of the implementation. An invariant held by convention rather than by structure; an API easy to use wrongly (a free-floating Option, representable invalid states, a missing #[must_use]); duplicated logic; a layer leak — pdn-layer knows nothing of data-layer, data-layer knows no capability semantics. Ask whether a call can be constructed in which the new mechanism silently does not apply.` },
  { key: 'rules', effort: 'low', prompt: `Repository rules. Read CLAUDE.md at the repository root, and crates/pdn-store/CLAUDE.md when the diff touches that crate. Denied lints (unwrap_used, panic, todo, unimplemented, print_*, dbg_macro; as_conversions and expect_used as warnings), max_width = 100, 80 lines per function, cognitive complexity 15. Comments: maximally brief and critical-only, present tense ONLY — no "legacy", "interim", "for now", "used to", "no longer", "until X lands", "future work", no review-finding references, no PR numbers; Dn references are banned in code (they live in the design doc), ADR-XXXX and Invariant N are fine. For anything under mia-docs: one physical line per paragraph, digits with thousands separators, no invented notation or abbreviations, scenario headings at exactly four hashes (a wrong level drops the scenario from validation in silence), the manual sweep the unvalidated main spec tree needs when a change removes or renames something. This is a checklist walked against a grep, not an investigation.` },
  { key: 'gaps', prompt: `What the change forgot. Observability and metrics — if the diff adds any, ask who can read them on a running node and what an incident still cannot see; a metric no surface exports is a mechanism with no reader. Rollback. Compatibility with data already written and with peers still on the previous protocol. Error paths, and whether a failure is distinguishable from a success or from unreachability at the place that observes it. Updating the spec to match the new behaviour.` },
  { key: 'operating-conditions', prompt: `What the change survives: it against mia-docs/openspec/specs/code-practices/operating-conditions.md, condition by condition — several identities on one node (a check that asks "is this our node" where it must ask "is this device one of this identity's"); one device against several (an assumption of a founder device; addressing assembled out of tickets, where a ticket names the device that minted it); a device linking before, during, or after the process; an unstable connection (a refusal indistinguishable from never reaching the peer; a half-completed act a retry cannot converge from; a timeout taken for an answer); a disk that fills (a swallowed write error, a replica reported as converged); a capability granted, narrowed, widened, revoked, and granted again over the same claim. For each condition: does it change the outcome of this change, and if it does, is there a scenario in the spec and a test behind that path. "It does not" is a legitimate answer, but a stated one, not an implied one.` },
]

// The two that fan over file groups. Splitting shortens the slowest angle,
// which is what the barrier waits for — and costs tokens, which is what the
// ceiling bounds. See the command's §2a on why these pull against each other.
const SPLITTABLE = [
  { key: 'correctness', prompt: `Correctness. Edge cases, range boundaries, wrap-around, inclusivity, arithmetic, order of operations, counter overflow, unhandled Option / Result paths, early returns that leave state half-built.` },
  { key: 'concurrency', prompt: `Concurrency. A guard held across an await; cancel safety, meaning what happens if the future is dropped at exactly this point; Drop paths and fire-and-forget delivery; races and delivery order; behaviour when a queue is full; what releases a resource when the happy path is not taken.` },
]

// Merge groups down until the phase fits under the ceiling. Merging, never
// dropping: a dropped group is a file nobody reads, and silence about it
// would read afterwards as coverage.
function fit(groups) {
  const room = Math.max(1, Math.floor((CEILING - WHOLE.length) / SPLITTABLE.length))
  const src = (groups && groups.length ? groups : [{ name: 'all', files: A.files || [] }])
  if (src.length <= room) return src
  const out = Array.from({ length: room }, () => ({ name: [], files: [] }))
  src.forEach((g, i) => {
    const b = out[i % room]
    b.name.push(g.name)
    b.files.push(...(g.files || []))
  })
  log(`${src.length} file groups merged into ${room} to stay under the ${CEILING}-agent search ceiling`)
  return out.map((b) => ({ name: b.name.join(' + '), files: b.files }))
}

const groups = fit(A.groups)
const LENSES = [
  ...SPLITTABLE.flatMap((l) =>
    groups.length === 1
      ? [{ ...l, prompt: `${l.prompt}\n\nFiles: ${(groups[0].files || []).join(', ')}.` }]
      : groups.map((g) => ({
          ...l,
          key: `${l.key}-${g.name}`,
          prompt: `${l.prompt}\n\nYour file group is ${g.name}: ${(g.files || []).join(', ')}. Another agent covers the rest of the diff under the same subject — stay in your group.`,
        })),
  ),
  ...WHOLE,
]

// A finding is whole when it leaves the angle: mechanism AND fixes. Splitting
// the fixes into a stage behind verification put them behind two barriers, and
// an early finalize then yields a file with no fixes at all. See the command's
// §2 on why that waste is paid deliberately.
const FIX_ITEM = {
  type: 'object',
  required: ['name', 'how', 'product', 'architecture'],
  properties: {
    name: { type: 'string' },
    how: { type: 'string' },
    product: { type: 'string' },
    architecture: { type: 'string' },
  },
}

const CLAIM_SCHEMA = {
  type: 'object',
  required: ['findings'],
  properties: {
    findings: {
      type: 'array',
      maxItems: 5,
      items: {
        type: 'object',
        required: ['title', 'file', 'line', 'severity', 'origin', 'symptom', 'cause', 'victim', 'evidence', 'fixes', 'recommendation'],
        properties: {
          title: { type: 'string' },
          file: { type: 'string' },
          line: { type: 'string' },
          severity: { type: 'string', enum: ['critical', 'medium', 'structural', 'cleanup'] },
          origin: { type: 'string', enum: ['new', 'pre-existing'] },
          symptom: { type: 'string' },
          cause: { type: 'string' },
          victim: { type: 'string' },
          evidence: { type: 'string' },
          fixes: { type: 'array', items: FIX_ITEM },
          recommendation: { type: 'string' },
        },
      },
    },
  },
}

const TRIAGE_SCHEMA = {
  type: 'object',
  required: ['groups'],
  properties: {
    groups: {
      type: 'array',
      items: {
        type: 'object',
        required: ['members', 'primary', 'title', 'severity'],
        properties: {
          members: { type: 'array', items: { type: 'integer' } },
          primary: { type: 'integer' },
          title: { type: 'string' },
          severity: { type: 'string', enum: ['critical', 'medium', 'structural', 'cleanup'] },
        },
      },
    },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['state', 'reason'],
  properties: {
    state: { type: 'string', enum: ['CONFIRMED', 'PLAUSIBLE', 'REFUTED'] },
    reason: { type: 'string' },
    lines: { type: 'string' },
    corrections: { type: 'string' },
  },
}

const LANG = A.language
  ? `\n\nWrite every prose field of your answer in ${A.language}. Identifiers, paths, and type names stay exactly as they are in the code. Never translate these domain terms: capability, connection metadata store / CMS, private metadata store / PMS, claim, lock, race, identity, audience, connection, binder, session, snapshot, ingress, egress.`
  : ''
const CTX = A.context ? `\n\nContext from the user, to be honoured verbatim:\n${A.context}\n` : ''
const NORUN = `\n\nOn running things. You ARE expected to run the one named test that settles a finding — \`just test -E 'test(<name>)'\`, or the scope-appropriate equivalent in a nested repository — whenever a verdict turns on whether a path is reachable or a test is vacuous. Read-only build queries are equally welcome and cost nothing: cargo tree -e features, cargo metadata, a grep over Cargo.toml. Runs like these are what separate a demonstrated finding from a plausible one, and the last three runs of this command executed nothing at all, which is the failure this paragraph exists to prevent. Never assert what a run or a feature lane WOULD produce without running it: either run it, or say in evidence that the claim was derived by reading.

What nobody does: build, lint, or format the tree — no cargo build / check / clippy / fmt, no just check / just fix, no running the suite whole. That belongs to implementation and to CI, and cargo serializes every agent on one artifact lock, which turns this fan-out back into a queue. Never modify files in the working tree.`
const BASE = `Diff: ${(A.diffPaths || []).join(', ')}. Changed files: ${(A.files || []).join(', ')}.
Read the sources around the diff, not the diff alone. Cite lines as they stand in the working tree.${CTX}${NORUN}${LANG}`

phase('Map')
const map = await agent(
  `${BASE}\n\nDescribe what this change does: two or three paragraphs — which mechanism it introduces, which seams it touches, which invariants it bears on.${A.intentDir ? ` The intent is written down in ${A.intentDir} — read it.` : ''} No judgements and no findings. This is the map every other agent receives.`,
  { label: 'map', phase: 'Map' },
)

phase('Search')
const searched = await parallel(
  LENSES.map((lens) => () =>
    agent(
      `${BASE}\n\nMap of the change:\n${map}\n\nYour angle is ${lens.key}. ${lens.prompt}\n\nReturn at most 5 findings, each with a mechanism you traced through the sources yourself. Do not return a guess with no line of code behind it. Five is a ceiling, not a target: return a finding only if you would advise a human to spend an hour of their working day on it, and return two rather than pad to five. What clears the bar — a wrong answer under some input, data that crosses where it must not, a resource that grows without bound, an invariant a future edit will break unknowingly, a test that would pass with the mechanism removed. What does not, unless it causes one of those — naming, phrasing, a comment, an import, a shape you would have written differently.\n\nFor each finding fill in: how it shows up (symptom and exact trigger), what causes it (the mechanism through the code, with lines), who suffers. In evidence, say what you actually checked it with — reading the code, running a test, a probe — and if it was not checked, say so plainly. Then the ways to fix it — one or more, and for each separately: what to do and the size of the edit, the consequences for the product (what changes in the scenarios, what becomes impossible), and the consequences for the architecture (which new constraint it introduces, which invariant appears or hardens, what cannot be done afterwards; if none, write that it introduces none). Then a recommendation: which option and why, or — where the fork needs a human decision — exactly which question is in front of them.`,
      { label: `search:${lens.key}`, phase: 'Search', schema: CLAIM_SCHEMA, effort: lens.effort },
    ),
  ),
)
const cands = searched
  .map((res, i) => ((res && res.findings) || []).map((f) => ({ ...f, lens: LENSES[i].key })))
  .flat()

phase('Triage')
const digest = cands.map((f, i) => ({ i, lens: f.lens, sev: f.severity, at: `${f.file}:${f.line}`, title: f.title, symptom: f.symptom }))
const triage = cands.length
  ? await agent(
      `${BASE}\n\nMap of the change:\n${map}\n\n${cands.length} candidates came back from ${LENSES.length} independent angles reading the same files, so the same defect arrives several times in different words and never at the same line. Group them by MECHANISM — the thing that is wrong — not by file, line, or wording. Two candidates naming one cause at two call sites are one group; two candidates at one line naming different causes are two groups.\n\nYou merge and rank. You do NOT judge: whether a mechanism is real is decided by verification after you, so never drop a candidate for being weak, wrong, or unlikely — only ever as a duplicate of another. A candidate you cannot confidently merge stays on its own. Every index must appear in exactly one group.\n\nFor each group return: the member indices, the index of the member whose write-up is the fullest and most accurate (its prose is kept verbatim, so choose it on quality of the mechanism traced), one merged title, and the highest severity among the members.\n\nCandidates:\n${JSON.stringify(digest)}`,
      { label: 'triage', phase: 'Triage', schema: TRIAGE_SCHEMA, effort: 'low' },
    )
  : { groups: [] }

const grouped = ((triage && triage.groups) || [])
  .map((g) => {
    const members = (g.members || []).map((i) => cands[i]).filter(Boolean)
    const base = cands[g.primary] || members[0]
    if (!base) return null
    return {
      ...base,
      title: g.title || base.title,
      severity: g.severity || base.severity,
      lenses: [...new Set(members.map((m) => m.lens))],
      found_by: Math.max(members.length, 1),
    }
  })
  .filter(Boolean)
const covered = new Set(((triage && triage.groups) || []).flatMap((g) => g.members || []))
const orphans = cands.filter((_, i) => !covered.has(i)).map((f) => ({ ...f, lenses: [f.lens], found_by: 1 }))

// Severity order, not triage order: the deadline decides where the run is cut,
// and this decides that what it cuts is what mattered least.
const distinct = [...grouped, ...orphans].sort((a, b) => (RANK[a.severity] ?? 9) - (RANK[b.severity] ?? 9))
log(`${cands.length} candidates → ${distinct.length} distinct mechanisms, critical first`)

phase('Verify')
const results = await pipeline(
  distinct,
  // Refute. Cleanup skips the vote entirely. Fixes travelled in with the
  // finding, so nothing runs behind this stage — a run cut here still has
  // a complete file to write.
  (f) => {
    const votes = VOTES[f.severity] ?? 3
    if (!votes) {
      return Promise.resolve({
        ...f,
        verdicts: [{ state: 'CONFIRMED', reason: 'cleanup level — checked by reading, no separate vote was taken' }],
      })
    }
    return parallel(
      Array.from({ length: votes }, (_, i) => () =>
        agent(
          `${BASE}\n\nTry to REFUTE this finding. When in doubt, answer REFUTED.\n\nTitle: ${f.title}\nPlace: ${f.file}:${f.line}\nSymptom: ${f.symptom}\nCause: ${f.cause}\n\nOpen the sources and check the mechanism line by line. Check whether it is already caught by something above or below in the stack, whether the path is reachable at all, and whether a neighbouring edit in the same diff already changed the behaviour. Running one named test is allowed and encouraged where it decides the verdict; so are read-only build queries (cargo tree, cargo metadata). If the finding is real but its wording overreaches or misattributes, return the sharpened wording in corrections — narrowing a finding is doing the job, not softening it. Pass ${i + 1}.`,
          { label: `verify:${f.file}#${i + 1}`, phase: 'Verify', schema: VERDICT_SCHEMA },
        ),
      ),
    ).then((vs) => ({ ...f, verdicts: vs.filter(Boolean) }))
  },
)

phase('Gaps')
const all = results.filter(Boolean)
const gaps = await agent(
  `${BASE}\n\nMap:\n${map}\n\nWhat was found:\n${JSON.stringify(all.map((f) => ({ t: f.title, f: f.file, s: f.severity })))}\n\nWhat stayed uncovered? Name: which file or seam of the diff nobody read; which finding went unverified; which subject was asking to be examined and was not; which property of the change cannot be confirmed without a run that was never made. Speak in subjects and seams ("concurrency was not examined", "no one read tables.rs"), never in agents, angles, or votes — this text reaches the reader. Do not invent new findings — name gaps in coverage.`,
  { label: 'gaps', phase: 'Gaps' },
)

return { map, findings: all, gaps, lenses: LENSES.map((l) => l.key) }
