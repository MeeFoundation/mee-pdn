---
name: openspec-propose
description: Create an implementation-ready OpenSpec change with proposal, design, delta specs, and tasks. Use when the user wants to describe a feature or fix and produce the complete OpenSpec artifact set before implementation.
---

# Propose an OpenSpec change

Work in `mia-docs`. Treat the user's text following `$openspec-propose` as the input: either a kebab-case change name or a description from which to derive one.

1. If the desired change is unclear, ask one open-ended question before writing anything.
2. Check whether `mia-docs/openspec/changes/<name>` exists. If it does, ask whether to continue it or choose another name.
3. Create the change:

   ```bash
   cd mia-docs && openspec new change "<name>"
   ```

4. Read the build graph:

   ```bash
   cd mia-docs && openspec status --change "<name>" --json
   ```

5. Track artifact progress with the current task plan. Build ready artifacts in dependency order until every artifact named by `applyRequires` is done. For each ready artifact:

   - Run `cd mia-docs && openspec instructions <artifact-id> --change "<name>" --json`.
   - Read every completed dependency file returned by the command.
   - Use `template` as the output structure and `instruction` as the authoring guide.
   - Apply `context` and `rules` as constraints; never copy those blocks into the artifact.
   - Write the artifact to `outputPath`, verify the file exists, and refresh status before continuing.
   - If a material requirement is genuinely ambiguous, ask a focused question and continue after the answer.

6. Run `cd mia-docs && openspec status --change "<name>"` and summarize the created artifacts, their location, and readiness for `$openspec-apply-change`.

Create every artifact required for implementation. Follow repository documentation rules in `CLAUDE.md`, including single-line Markdown paragraphs, exact `#### Scenario:` headings, digits for numbers, and no undefined notation. Ground scenarios in checked behavior rather than plausible assumptions.
