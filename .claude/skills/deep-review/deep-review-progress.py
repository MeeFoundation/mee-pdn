#!/usr/bin/env python3
"""Progress counter for the /deep-review workflow, and the run's only clock.

A workflow script cannot read the time — Date.now() throws inside one, so that
a resumed run replays identically. So the budget lives here: this prints one
status line per call, and a FINALIZE-NOW line once the deadline has passed.

    deep-review-progress.py <transcriptDir> <numLenses> <T0 epoch> <deadline s>

Counts come from "type":"result" lines in journal.jsonl, never from "started" —
that one is written when an agent gets a concurrency slot, not when the script
asks for it, so everything queued behind the cap is missing from it.

Phases are told apart by the shape of the result field:
    {"findings": [...]}  a search angle
    {"groups":   [...]}  triage
    {"state":     ...}   a verifier
    a bare string        the map or the gap sweep
"""

import json
import os
import sys
import time

COST = {'critical': 5, 'medium': 3, 'structural': 3, 'cleanup': 0}


def read(journal):
    """Return (search_done, verify_done, findings, criticals, votes,
    triaged, mechanisms, triaged_criticals, triaged_votes)."""
    sdone = vdone = found = crit = votes = 0
    tn = tcrit = tvotes = 0
    triaged = False
    try:
        with open(journal) as fh:
            for line in fh:
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                if rec.get('type') != 'result':
                    continue
                r = rec.get('result')
                if not isinstance(r, dict):
                    continue
                if 'findings' in r:
                    sdone += 1
                    for f in r['findings']:
                        found += 1
                        sev = f.get('severity')
                        crit += sev == 'critical'
                        votes += COST.get(sev, 3)
                elif 'groups' in r:
                    triaged = True
                    for g in r['groups']:
                        tn += 1
                        sev = g.get('severity')
                        tcrit += sev == 'critical'
                        tvotes += COST.get(sev, 3)
                elif 'state' in r:
                    vdone += 1
    except OSError:
        pass
    return sdone, vdone, found, crit, votes, triaged, tn, tcrit, tvotes


def main():
    d, nl, t0, deadline = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
    secs = int(time.time()) - t0
    sdone, vdone, found, crit, votes, triaged, tn, tcrit, tvotes = read(
        os.path.join(d, 'journal.jsonl')
    )

    if triaged:
        # After triage the verify denominator is exact: it is the merged groups.
        body = (f"search {sdone}/{nl} · verify {vdone}/{tvotes} · "
                f"{tn} mechanisms from {found} findings ({tcrit} critical)")
    else:
        # Before it, the denominator is an upper bound over unmerged findings,
        # and it keeps climbing while angles are still out — hence the '+'.
        grow = '' if sdone >= nl else '+'
        tail = f" · findings {found} ({crit} critical)" if found else ''
        body = f"search {sdone}/{nl} · verify {vdone}/{votes}{grow} (pre-triage upper bound){tail}"

    print(f"{secs // 60}m {body}")
    if secs >= deadline:
        print(f"FINALIZE-NOW budget reached at {secs // 60}m — {body}")


if __name__ == '__main__':
    main()
