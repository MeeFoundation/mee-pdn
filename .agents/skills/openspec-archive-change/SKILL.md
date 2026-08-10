---
name: openspec-archive-change
description: Validate, synchronize, and archive a completed OpenSpec change. Use when the user wants to finalize an implemented change, merge delta specs into the main spec tree, or move a change into the dated archive.
---

# Archive an OpenSpec change

Archiving is destructive movement of project artifacts. Resolve the exact change and archive target before moving anything.

1. Use an explicitly supplied change name. If none is supplied, run `cd mia-docs && openspec list --json`, show active changes, and ask the user to select one; never guess.
2. Run `cd mia-docs && openspec status --change "<name>" --json`. Report incomplete artifacts and ask for confirmation before continuing with them.
3. Inspect the task artifact and count `- [ ]` versus `- [x]`. If tasks remain, report them and ask for confirmation before continuing. Proceed without this warning when no task file exists.
4. Run the mandatory parser gate before archiving:

   ```bash
   cd mia-docs && openspec validate --all --strict
   ```

   Stop on failure. Also inspect every delta scenario heading for exact `#### Scenario:` form because the parser can silently miss a wrong heading level.
5. If `mia-docs/openspec/changes/<name>/specs/` exists, compare every delta with its destination in the main spec tree. Summarize additions, modifications, removals, and renames. Sweep the entire main spec tree and other active changes for removed or renamed concepts, and keep `architecture/language/` synchronized.
6. Ask whether to synchronize when deltas would change the main tree. If requested, apply the delta changes directly and verify them; do not delegate this step to an unavailable Claude-only skill. If already synchronized, offer archive, synchronize again, or cancel.
7. Compute `YYYY-MM-DD-<name>` from the current date. Verify that `mia-docs/openspec/changes/archive/YYYY-MM-DD-<name>` does not exist. If it exists, stop and report the collision.
8. Create the archive directory if necessary and move only the resolved change directory:

   ```bash
   mkdir -p mia-docs/openspec/changes/archive
   mv mia-docs/openspec/changes/<name> mia-docs/openspec/changes/archive/YYYY-MM-DD-<name>
   ```

9. Verify the destination and report the change name, schema, archive path, synchronization result, validation result, and any accepted warnings.

Preserve `.openspec.yaml`. Do not perform Git staging, commits, pushes, or checkouts.
