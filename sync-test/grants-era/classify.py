#!/usr/bin/env -S python3 -I
"""Classifies a sync-test run directory: writes report.txt in it, appends one row to runs.csv, exits 0 PASS, 1 FAIL, 2 INCONCLUSIVE.

The rules are design.html's "Classifying every entry" and "Metrics" as plan.md amends them: arrival is ordered
against a session by the reader's event stream, and the two sides of a session are paired by their `session` id,
then checked by sent and received counts. The run directory layout is plan.md's
"Run directory"; meta.json carries `t0_us`, `nodes` (name to `node` and `issuer`), `readers` (name to
`interval_s` and `covers`, writer to "all" or a list of paths), `phases`, optional `cut` and `cpu_cores`.
A payload's `sha256` is the hex SHA-256 of its other fields as compact JSON with sorted keys.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import csv
import hashlib
import json
import math
import statistics
from pathlib import Path

STALL_INTERVALS = 5
CPU_LIMIT = 0.9
CLOCK_BOUND_US = 20_000
UNDETERMINED_LIMIT = 0.01
DEFECTS = ('violated', 'lost', 'content_lost', 'split', 'corrupt', 'extra', 'stale')
COUNTS = ('expected', 'readable', 'violated', 'late', 'lost', 'content_lost', 'stalled_pairs', 'stalled_entries',
          'split', 'teardown_split', 'undetermined', 'undetermined_absent', 'corrupt', 'extra', 'stale',
          'refused', 'unlisted', 'no_latency', 'clock_unchecked')
EXIT = {'PASS': 0, 'FAIL': 1, 'INCONCLUSIVE': 2}


def load_jsonl(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []


def payload_sha(fields):
    return hashlib.sha256(json.dumps(fields, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def payload_ok(text):
    try:
        fields = json.loads(text)
        return fields.pop('sha256') == payload_sha(fields)
    except (ValueError, KeyError, AttributeError, TypeError):
        return False


def is_gap(prev, rec):
    """Events were lost between prev and rec: `lagged`, a new `sub`, or a `seq` that does not follow."""
    if rec['kind'] == 'lagged':
        return True
    if prev is None:
        return rec['seq'] != 0
    return rec['sub'] != prev['sub'] or rec['seq'] != prev['seq'] + 1


def streams(records):
    """One node's events per issuer in received order, with the positions at which events were lost."""
    out, last = {}, {}
    for rec in records:
        recs, gaps = out.setdefault(rec['issuer'], ([], []))
        if is_gap(last.get(rec['issuer']), rec):
            gaps.append(len(recs))
        last[rec['issuer']] = rec
        recs.append(rec)
    return out


def sessions(recs, peer, origin):
    return [(i, r) for i, r in enumerate(recs)
            if r['kind'] == 'sync_finished' and r['peer'] == peer and r['origin'] == origin]


def pair_sides(rs, ws):
    """Reader-dialed and writer-accepted sides with equal `session`, in the reader's stream order."""
    by_id = {w['session']: (p, w) for p, w in ws if w.get('session')}
    return [((p, r), by_id[r['session']]) for p, r in rs if r.get('session') in by_id]


def counts_match(r, w):
    """A failed side may carry no counts; only two present counts can disagree."""
    return r.get('received') is None or w.get('sent') is None or r['received'] == w['sent']


def qualifies(r, w, t_write):
    return r['ok'] and w['ok'] and w['started'] > t_write


def arrived_before(arr, pos):
    return arr is not None and arr < pos


def is_teardown(r, w):
    failing = w if r['ok'] else r
    return (failing.get('error_kind') == 'close' and r.get('received') is not None
            and w.get('sent') is not None and r['received'] == w['sent'])


def deadline(last_write, interval_s):
    return last_write + STALL_INTERVALS * interval_s * 1_000_000


def expected_writes(journal):
    """Per writer and path, the one write a reader must hold: an ok s/ write, the last ok version of an h/ slot."""
    last = {}
    for j in journal:
        key = (j['writer'], j['path'])
        if j['result'] == 'ok' and (key not in last or j.get('version', 0) > last[key].get('version', 0)):
            last[key] = j
    return list(last.values())


def covered(cover, path):
    return cover == 'all' or path in cover


def final_state(item, e, older):
    if item is None:
        return 'absent'
    if not item.get('present') or item.get('payload') is None:
        return 'content_lost'
    if item['hash'] == e['hash']:
        return 'readable' if payload_ok(item['payload']) else 'corrupt'
    return 'stale' if item['hash'] in older else 'corrupt'


def absent_state(has_qualifying, broken):
    """An absent entry is lost only once a qualifying session exists; elapsed time never decides it."""
    if has_qualifying:
        return 'lost'
    return 'undetermined_absent' if broken else 'stalled_entries'


def in_clock_window(t_arrival, t_write, w_finished, bound):
    return t_write - bound <= t_arrival <= w_finished + bound


def nearest_rank(values, p):
    return values[max(1, math.ceil(p / 100 * len(values))) - 1]


def offset(samples, t):
    if not samples:
        return 0, 0
    s = min(samples, key=lambda s: abs(s['t'] - t))
    return s['offset_us'], s['bound_us']


def classify(run):
    meta = json.loads((run / 'meta.json').read_text())
    t0 = meta['t0_us']
    journal = load_jsonl(run / 'journal.jsonl')
    clock = load_jsonl(run / 'clock.jsonl')
    health = load_jsonl(run / 'health.jsonl')
    nodes = meta['nodes']
    writer_of = {v['issuer']: k for k, v in nodes.items()}
    events = {p.stem: streams(load_jsonl(p)) for p in sorted((run / 'events').glob('*.jsonl'))}
    listings = {p.stem: json.loads(p.read_text()) for p in sorted((run / 'listing').glob('*.json'))}

    def fmt(t):
        return f'{(t - t0) / 1e6:.3f}'

    def sides(W, R, pair):
        (_, r), (_, w) = pair
        verdict = {True: 'ok', False: 'error'}
        return (f'{W} accepted {fmt(w["started"])}-{fmt(w["finished"])} {verdict[w["ok"]]}, '
                f'{R} dialed {fmt(r["started"])}-{fmt(r["finished"])} {verdict[r["ok"]]} ({R} clock)'
                + ''.join(f' [{s} {x.get("error_kind")}: {x.get("error")}]' for s, x in ((W, w), (R, r)) if not x['ok']))

    c = dict.fromkeys(COUNTS, 0)
    c['refused'] = sum(j['result'] != 'ok' for j in journal)
    exp = expected_writes(journal)
    older = {}
    for j in journal:
        if j['result'] == 'ok':
            older.setdefault((j['writer'], j['path']), set()).add(j['hash'])
    for e in exp:
        older.get((e['writer'], e['path']), set()).discard(e['hash'])
    last_write = {}
    for j in journal:
        if j['result'] == 'ok':
            last_write[j['writer']] = max(last_write.get(j['writer'], 0), j['t_write'])

    defects, notes, health_lines, stalled_lines = [], [], [], []
    latency, clock_bad, expected_at = {}, {}, {}
    recovery = {}
    cut = meta.get('cut')
    phases = meta.get('phases', [])

    for R, rc in meta['readers'].items():
        samples = [s for s in clock if s['reader'] == R]
        listing = listings.get(R)
        if listing is None:
            notes.append(f'reader {R} has no final listing: its pairs are counted unlisted, neither readable nor lost (design silent)')
        rec = recovery.setdefault(R, {'first': None, 'cut_ready': [], 'cut_expected': 0, 'ready': [], 'writes': [], 'running': []})
        for W, cover in rc['covers'].items():
            wi = nodes[W]['issuer']
            rrecs, rgaps = events.get(R, {}).get(wi, ([], []))
            wrecs, wgaps = events.get(W, {}).get(wi, ([], []))
            rs = sessions(rrecs, nodes[W]['node'], 'connect')
            ws = sessions(wrecs, nodes[R]['node'], 'accept')
            ordered = pair_sides(rs, ws)
            n = len(ordered)
            # Pairs from the first lost event or unmatched count on are not trusted.
            cuts = [next((i for i, ((rp, _), _) in enumerate(ordered) if rp >= g), n) for g in rgaps]
            cuts += [next((i for i, (_, (wp, _)) in enumerate(ordered) if wp >= g), n) for g in wgaps]
            cuts += [i for i, ((_, r), (_, w)) in enumerate(ordered) if not counts_match(r, w)][:1]
            broken = bool(cuts)
            pairs = ordered[:min(cuts, default=n)]
            by_rpos = {rp: i for i, ((rp, _), _) in enumerate(pairs)}

            for pair in pairs:
                (_, r), (_, w) = pair
                if r['ok'] != w['ok']:
                    if is_teardown(r, w):
                        c['teardown_split'] += 1
                    else:
                        c['split'] += 1
                        defects.append(f'SPLIT {W} -> {R}: {sides(W, R, pair)}')
                if cut and w['started'] < cut['start_us'] < w['finished']:
                    rec['running'].append(f'{W} {fmt(w["started"])}-{fmt(w["finished"])}')
                if cut and r['ok'] and w['ok'] and w['started'] > cut['end_us']:
                    t = w['started'] - cut['end_us']
                    rec['first'] = t if rec['first'] is None else min(rec['first'], t)

            # Health covers every pair; findings come from trusted pairs only.
            both_ok = [w['started'] for (_, r), (_, w) in ordered if r['ok'] and w['ok']]
            gaps = [b - a for a, b in zip(both_ok, both_ok[1:])]
            wdials = sessions(wrecs, nodes[R]['node'], 'connect')
            errors = sorted({x.get('error') or '' for _, x in rs + ws + wdials if not x['ok']})
            health_lines.append(
                f'  {W} -> {R}: dialed by {R}: {len(both_ok)} ok, {sum(not r["ok"] and not w["ok"] for (_, r), (_, w) in ordered)} failed, '
                f'{sum(r["ok"] != w["ok"] for (_, r), (_, w) in ordered)} split, one-sided: {len(rs) - n} {R} only, {len(ws) - n} {W} only; '
                f'{n - len(pairs)} untrusted; median gap '
                + (f'{statistics.median(gaps) / 1e6:.1f} s' if gaps else '-')
                + f'; dialed by {W}: {sum(x["ok"] for _, x in wdials)} ok, {sum(not x["ok"] for _, x in wdials)} failed'
                + (f'; errors: {"; ".join(errors)}' if errors else ''))

            stalled = False
            if W in last_write:
                dl = deadline(last_write[W], rc['interval_s'])
                found = any(qualifies(r, w, last_write[W]) and w['started'] <= dl for (_, r), (_, w) in pairs)
                stalled = not found and not broken
                c['stalled_pairs'] += stalled

            for e in exp:
                if e['writer'] != W or not covered(cover, e['path']):
                    continue
                expected_at.setdefault(R, set()).add((wi, e['path']))
                c['expected'] += 1
                arr = next((i for i, x in enumerate(rrecs) if x['kind'] == 'insert_remote'
                            and x['path'] == e['path'] and x['hash'] == e['hash']), None)
                q = next((i for i, ((_, r), (_, w)) in enumerate(pairs) if qualifies(r, w, e['t_write'])), None)
                violated = q is not None and not arrived_before(arr, pairs[q][0][0])
                c['violated'] += violated
                c['undetermined'] += q is None and broken
                if listing is None:
                    state = 'unlisted'
                else:
                    state = final_state(listing.get(wi, {}).get(e['path']), e, older.get((W, e['path']), set()))
                    if state == 'absent':
                        state = absent_state(q is not None, broken)
                c[state] += 1
                if state == 'stalled_entries':
                    stalled_lines.append(f'STALLED {W} {e["path"]} -> {R}: t_write {fmt(e["t_write"])}; absent, no qualifying session')
                c['late'] += violated and state == 'readable'
                in_cut = cut and cut['start_us'] <= e['t_write'] < cut['end_us']
                rec['writes'].append(e['t_write'])
                rec['cut_expected'] += bool(in_cut)
                if violated or state in DEFECTS:
                    tag = '+'.join(t for t, on in (('VIOLATED', violated), (state.upper().replace('_', ' '), state in DEFECTS)) if on)
                    if violated and state == 'readable':
                        tag += '+LATE'
                    where = sides(W, R, pairs[q]) if q is not None else 'no qualifying session'
                    defects.append(f'{tag} {W} {e["path"]} -> {R}: t_write {fmt(e["t_write"])}; {where}')
                if state != 'readable':
                    continue
                if arr is None:
                    c['no_latency'] += 1
                    continue
                ins = rrecs[arr]
                t_ready = ins['t'] if ins.get('content_status') == 'complete' else next(
                    (x['t'] for x in rrecs[arr:] if x['kind'] == 'content_ready' and x['hash'] == e['hash']), None)
                if t_ready is None:
                    c['no_latency'] += 1
                    continue
                off, bound = offset(samples, t_ready)
                phase = next((p['name'] for p in phases if p['start_us'] <= e['t_write'] < p['end_us']), 'unphased')
                latency.setdefault((phase, R), []).append(((t_ready - off - e['t_write']) / 1e6, bound))
                rec['ready'].append(t_ready - off)
                if in_cut:
                    rec['cut_ready'].append(t_ready - off)
                if samples:
                    nxt = next((i for i in range(arr, len(rrecs)) if rrecs[i]['kind'] == 'sync_finished'
                                and rrecs[i]['peer'] == nodes[W]['node']), None)
                    carrier = by_rpos.get(nxt)
                    if carrier is None:
                        c['clock_unchecked'] += 1
                    elif not in_clock_window(ins['t'] - off, e['t_write'], pairs[carrier][1][1]['finished'], bound):
                        clock_bad[R] = clock_bad.get(R, 0) + 1

    for R in meta['readers']:
        for iss, items in (listings.get(R) or {}).items():
            for path in items:
                if (iss, path) not in expected_at.get(R, set()):
                    c['extra'] += 1
                    defects.append(f'EXTRA {writer_of.get(iss, iss)} {path} -> {R}: held outside the expected set')

    clock_invalid = set()
    for R in meta['readers']:
        samples = [s for s in clock if s['reader'] == R]
        if clock_bad.get(R) or any(s['bound_us'] >= CLOCK_BOUND_US for s in samples):
            clock_invalid.add(R)

    reasons = []
    found = {k: c[k] for k in DEFECTS if c[k]}
    cpu = [h['cpu_pct'] for h in health if 'cpu_pct' in h]
    background = [h['app_state'] for h in health if h.get('app_state', 'active') != 'active']
    undetermined_share = c['undetermined'] / c['expected'] if c['expected'] else 0
    if c['stalled_pairs']:
        reasons.append(f'stalled pairs {c["stalled_pairs"]}')
    if undetermined_share > UNDETERMINED_LIMIT:
        reasons.append(f'undetermined {undetermined_share:.1%} of expected')
    if c['unlisted']:
        reasons.append(f'unlisted pairs {c["unlisted"]}')
    if background:
        reasons.append('the app left the foreground')
    verdict = 'FAIL' if found else 'INCONCLUSIVE' if reasons else 'PASS'

    head = {'FAIL': ', '.join(f'{v} {k.replace("_", " ")}' for k, v in found.items()), 'INCONCLUSIVE': ', '.join(reasons), 'PASS': ''}[verdict]
    out = [f'VERDICT: {verdict}' + (f' — {head}' if head else ''), f'run {run}']
    out.append(f'expected {c["expected"]:,} · readable {c["readable"]:,} · stalled pairs {c["stalled_pairs"]} '
               f'(absent entries under them {c["stalled_entries"]}) · split sessions {c["split"]} · undetermined {c["undetermined"]}')
    out.append(f'violated {c["violated"]} · late after violation {c["late"]} · lost {c["lost"]} · content lost {c["content_lost"]} · '
               f'corrupt {c["corrupt"]} · extra {c["extra"]} · stale {c["stale"]} · refused writes {c["refused"]} · '
               f'teardown splits {c["teardown_split"]} · unlisted {c["unlisted"]} · undetermined absent {c["undetermined_absent"]}')
    out.append(f'readable without latency (arrival or content_ready not in the stream): {c["no_latency"]}')
    summary = []
    for (phase, R), vals in sorted(latency.items()):
        v = sorted(x for x, _ in vals)
        pct = ' · '.join(f'p{p} {nearest_rank(v, p):.3f} s' for p in (50, 90, 99))
        mark = ' INVALID clock' if R in clock_invalid else ''
        out.append(f'latency, {phase}, {R}: {pct} · max {v[-1]:.3f} s (n {len(v)}, clock ±{max(b for _, b in vals) / 1000:.0f} ms){mark}')
        summary.append(f'{phase}/{R}:{nearest_rank(v, 50):.3f}/{nearest_rank(v, 90):.3f}/{nearest_rank(v, 99):.3f}')
    rec_out = {}
    if cut:
        notes.append('time to first session is taken from the writer side\'s started (design silent on which side)')
        for R, rec in recovery.items():
            first = None if rec['first'] is None else rec['first'] / 1e6
            catch = (max(rec['cut_ready']) - cut['end_us']) / 1e6 if rec['cut_ready'] and len(rec['cut_ready']) == rec['cut_expected'] else None
            conv = (max(rec['ready']) - max(rec['writes'])) / 1e6 if rec['ready'] and rec['writes'] else None
            rec_out[R] = (first, catch, conv)
            show = [('first session', first), ('catch-up', catch), ('converge', conv)]
            out.append(f'recovery, {R}: ' + ' · '.join(f'{k} ' + ('-' if v is None else f'{v:.3f} s') for k, v in show)
                       + (f' · sessions running at the cut: {", ".join(rec["running"])}' if rec['running'] else ''))
            if rec['cut_ready'] and catch is None:
                out.append(f'  catch-up of {R} not computed: {rec["cut_expected"] - len(rec["cut_ready"])} partition entries have no readable time')
    else:
        out.append('recovery: no cut in this run')
    out.append('session health:')
    out += health_lines
    cores = meta.get('cpu_cores')
    if cpu and cores:
        peak = max(cpu)
        out.append(f'validity: host CPU peak {peak:.0f}% of {cores * 100}%' + (' — latency host-bound' if peak >= CPU_LIMIT * cores * 100 else ' ok'))
    else:
        out.append('validity: host CPU not recorded')
    out.append(f'validity: event loss {undetermined_share:.2%} undetermined ' + ('ok' if undetermined_share <= UNDETERMINED_LIMIT else '— INCONCLUSIVE'))
    for R in meta['readers']:
        samples = [s for s in clock if s['reader'] == R]
        if samples:
            out.append(f'validity: clock of {R}: worst bound {max(s["bound_us"] for s in samples) / 1000:.1f} ms, '
                       f'{clock_bad.get(R, 0)} arrivals outside their window' + (' — latencies INVALID' if R in clock_invalid else ' ok'))
        else:
            out.append(f'validity: clock of {R}: no samples, offset 0, not checked')
    out.append('validity: foreground ' + (f'left: {", ".join(background)} — run invalid' if background else 'ok'))
    if c['clock_unchecked']:
        notes.append(f'{c["clock_unchecked"]} arrivals not clock-checked: the session carrying them is writer-dialed or unpaired (design silent)')
    if c['undetermined_absent']:
        notes.append(f'{c["undetermined_absent"]} absent pairs with lost events are not called lost: lost needs a known qualifying session')
    if c['teardown_split']:
        notes.append('teardown splits are counted, not defects: a teardown error fails its side because a clean end of stream can also come from a dropped session')
    out += [f'note: {n}' for n in notes]
    out += defects + stalled_lines

    row = {'run_id': meta.get('run_id', run.name), 'verdict': verdict, **c, 'latency': ' '.join(summary),
           'clock_invalid': ' '.join(sorted(clock_invalid)), 'cpu_peak': max(cpu) if cpu else ''}
    return {'verdict': verdict, 'counts': c, 'latency': {k: [x for x, _ in v] for k, v in latency.items()},
            'clock_invalid': clock_invalid, 'recovery': rec_out, 'report': '\n'.join(out) + '\n', 'row': row}


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('run', type=Path)
    ap.add_argument('--runs-csv', type=Path, help='default: runs.csv in the run directory\'s parent')
    args = ap.parse_args()
    res = classify(args.run)
    (args.run / 'report.txt').write_text(res['report'])
    path = args.runs_csv or args.run.parent / 'runs.csv'
    new = not path.exists()
    with path.open('a', newline='') as f:
        w = csv.DictWriter(f, fieldnames=list(res['row']))
        if new:
            w.writeheader()
        w.writerow(res['row'])
    sys.stdout.write(res['report'])
    return EXIT[res['verdict']]


if __name__ == '__main__':
    sys.exit(main())
