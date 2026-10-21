#!/usr/bin/env -S python3 -I
"""Classifies a pod sync-test run directory: writes report.txt in it, appends one row to runs.csv, exits 0 PASS, 1 FAIL, 2 INCONCLUSIVE.

The rules are design.md's "Classifying every entry" and "Metrics". The run directory layout is plan.md's "Run directory":
meta.json carries `t0_us`, `nodes` (name to `node`), `readers` (name to `pass_interval_s`), optional `removed` (`name` and
the removal's membership-store `key`), `phases`, optional `cut` and `cpu_cores`. Each node's events are one stream covering
both stores of the pod, every event naming its `store`. A listing is a list of entries `{key, author, hash, len, present,
payload, verdict}`: a key holds one entry per author, so it is never reduced to a map by key.
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
RECORDS, MEMBERSHIP = 'records', 'membership'
DEFECTS = ('violated', 'lost', 'content_lost', 'uncounted', 'split', 'corrupt', 'extra')
COUNTS = ('expected', 'readable', 'violated', 'late', 'lost', 'content_lost', 'uncounted', 'stalled_entries',
          'stalled_readers', 'split', 'teardown_split', 'undetermined', 'undetermined_absent', 'corrupt', 'extra',
          'refused', 'unlisted', 'via_relay', 'no_latency', 'clock_unchecked')
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


def stream(records):
    """One node's events in received order, and the positions at which events were lost."""
    gaps, prev = [], None
    for i, rec in enumerate(records):
        if is_gap(prev, rec):
            gaps.append(i)
        prev = rec
    return records, gaps


def finished(recs, store, peer=None):
    return [(i, r) for i, r in enumerate(recs) if r['kind'] == 'sync_finished' and r['store'] == store
            and (peer is None or r['peer'] == peer)]


def held_at(recs, store, key):
    """When this node first held `key`: its own entry timestamp for an insert it wrote, else the observed arrival."""
    for i, r in enumerate(recs):
        if r['kind'] in ('insert_local', 'insert_remote') and r['store'] == store and r['key'] == key:
            return i, (r['entry_ts'] if r['kind'] == 'insert_local' else r['t'])
    return None


def pair_sides(rs, ps):
    """The reader's sides and a peer's sides with equal `session`, in the reader's stream order."""
    by_id = {p['session']: (i, p) for i, p in ps if p.get('session')}
    return [((i, r), by_id[r['session']]) for i, r in rs if r.get('session') in by_id]


def counts_match(r, p):
    """A failed side may carry no counts; only two present counts can disagree."""
    return r.get('received') is None or p.get('sent') is None or r['received'] == p['sent']


def qualifies(r, p, held):
    """Both sides ok, and the serving side started after it held the entry, both on its own clock."""
    return r['ok'] and p['ok'] and held is not None and p['started'] > held


def arrived_before(arr, pos):
    return arr is not None and arr < pos


def is_teardown(r, p):
    failing = p if r['ok'] else r
    return (failing.get('error_kind') == 'close' and r.get('received') is not None
            and p.get('sent') is not None and r['received'] == p['sent'])


def deadline(last_write, interval_s):
    return last_write + STALL_INTERVALS * interval_s * 1_000_000


def expected_writes(journal):
    """Every write that answered ok: nothing in a pod is replaced, so each is expected on every reader."""
    return [j for j in journal if j['result'] == 'ok']


def entry_of(items, e):
    """The listed entry an expected write is: its key with its hash, else any entry at its key."""
    at_key = [i for i in items if i['key'] == e['key']]
    return next((i for i in at_key if i['hash'] == e['hash']), at_key[0] if at_key else None)


def final_state(item, e):
    if item is None:
        return 'absent'
    if not item.get('present') or item.get('payload') is None:
        return 'content_lost'
    if item['hash'] != e['hash'] or not payload_ok(item['payload']):
        return 'corrupt'
    return 'readable' if item.get('verdict') == 'counted' else 'uncounted'


def absent_state(has_qualifying, broken):
    """An absent entry is lost only once a qualifying session exists; elapsed time never decides it."""
    if has_qualifying:
        return 'lost'
    return 'undetermined_absent' if broken else 'stalled_entries'


def served_after_remove(held_remove, p):
    """The serving side held the removal before its side started: it served a member it knew was gone."""
    return held_remove is not None and p['started'] > held_remove


def in_clock_window(t_arrival, t_write, p_finished, bound):
    return t_write - bound <= t_arrival <= p_finished + bound


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
    name_of = {v['node']: k for k, v in nodes.items()}
    events = {p.stem: stream(load_jsonl(p)) for p in sorted((run / 'events').glob('*.jsonl'))}
    listings = {p.stem: json.loads(p.read_text()) for p in sorted((run / 'listing').glob('*.json'))}

    def recs_of(n):
        return events.get(n, ([], []))

    def fmt(t):
        return f'{(t - t0) / 1e6:.3f}'

    def sides(P, R, pair):
        (_, r), (_, p) = pair
        verdict = {True: 'ok', False: 'error'}
        origin = {'accept': 'accepted', 'connect': 'dialed'}
        return (f'{P} {origin[p["origin"]]} {fmt(p["started"])}-{fmt(p["finished"])} {verdict[p["ok"]]}, '
                f'{R} {origin[r["origin"]]} {fmt(r["started"])}-{fmt(r["finished"])} {verdict[r["ok"]]} ({R} clock)'
                + ''.join(f' [{s} {x.get("error_kind")}: {x.get("error")}]' for s, x in ((P, p), (R, r)) if not x['ok']))

    c = dict.fromkeys(COUNTS, 0)
    c['refused'] = sum(j['result'] != 'ok' for j in journal)
    exp = expected_writes(journal)
    last_write = max((e['t_write'] for e in exp), default=0)
    held = {}

    def holds(P, key):
        if (P, key) not in held:
            found = held_at(recs_of(P)[0], RECORDS, key)
            held[(P, key)] = found and found[1]
        return held[(P, key)]

    defects, notes, health_lines, stalled_lines = [], [], [], []
    latency, clock_bad = {}, {}
    recovery = {}
    cut = meta.get('cut')
    phases = meta.get('phases', [])

    for R, rc in meta['readers'].items():
        samples = [s for s in clock if s['reader'] == R]
        rrecs, rgaps = recs_of(R)
        listing = listings.get(R)
        if listing is None:
            notes.append(f'reader {R} has no final listing: its entries are counted unlisted, neither readable nor lost')
        rec = recovery.setdefault(R, {'first': None, 'cut_ready': [], 'cut_expected': 0, 'ready': [], 'running': []})

        # Every trusted pair of sides with every member, in R's stream order.
        pairs, broken = [], bool(rgaps)
        for P, pn in nodes.items():
            if P == R:
                continue
            precs, pgaps = recs_of(P)
            rs = finished(rrecs, RECORDS, pn['node'])
            ps = finished(precs, RECORDS, nodes[R]['node'])
            ordered = pair_sides(rs, ps)
            n = len(ordered)
            # Pairs from the first lost event or unmatched count on are not trusted.
            cuts = [next((i for i, ((rp, _), _) in enumerate(ordered) if rp >= g), n) for g in rgaps]
            cuts += [next((i for i, (_, (pp, _)) in enumerate(ordered) if pp >= g), n) for g in pgaps]
            cuts += [i for i, ((_, r), (_, p)) in enumerate(ordered) if not counts_match(r, p)][:1]
            trusted = ordered[:min(cuts, default=n)]
            broken = broken or len(trusted) < n
            pairs += [(P, pair) for pair in trusted]

            for pair in trusted:
                (_, r), (_, p) = pair
                if r['ok'] != p['ok']:
                    if is_teardown(r, p):
                        c['teardown_split'] += 1
                    else:
                        c['split'] += 1
                        defects.append(f'SPLIT {P} - {R}: {sides(P, R, pair)}')
                if cut and p['started'] < cut['start_us'] < p['finished']:
                    rec['running'].append(f'{P} {fmt(p["started"])}-{fmt(p["finished"])}')
                if cut and r['ok'] and p['ok'] and p['started'] > cut['end_us']:
                    t = p['started'] - cut['end_us']
                    rec['first'] = t if rec['first'] is None else min(rec['first'], t)

            both_ok = [p['started'] for (_, r), (_, p) in ordered if r['ok'] and p['ok']]
            gaps = [b - a for a, b in zip(both_ok, both_ok[1:])]
            mem = finished(rrecs, MEMBERSHIP, pn['node'])
            if rs or ps:
                errors = sorted({x.get('error') or '' for _, x in rs + ps if not x['ok']})
                health_lines.append(
                    f'  {P} - {R}: records {len(both_ok)} ok, {sum(not r["ok"] and not p["ok"] for (_, r), (_, p) in ordered)} failed, '
                    f'{sum(r["ok"] != p["ok"] for (_, r), (_, p) in ordered)} split, one-sided: {len(rs) - n} {R} only, '
                    f'{len(ps) - n} {P} only; dialed by {R} {sum(r["origin"] == "connect" for (_, r), _ in ordered)}; '
                    f'{n - len(trusted)} untrusted; membership sessions {len(mem)}; median gap '
                    + (f'{statistics.median(gaps) / 1e6:.1f} s' if gaps else '-')
                    + (f'; errors: {"; ".join(errors)}' if errors else ''))
        pairs.sort(key=lambda x: x[1][0][0])

        if exp and not broken and not any(
                r['ok'] and p['ok'] and last_write < p['started'] <= deadline(last_write, rc['pass_interval_s'])
                for _, ((_, r), (_, p)) in pairs):
            c['stalled_readers'] += 1
            stalled_lines.append(f'STALLED READER {R}: no ok record session started within {STALL_INTERVALS} pod intervals after the last write')

        for e in exp:
            c['expected'] += 1
            arr = next((i for i, x in enumerate(rrecs) if x['kind'] == 'insert_remote' and x['store'] == RECORDS
                        and x['key'] == e['key'] and x['hash'] == e['hash']), None)
            q = next(((P, pair) for P, pair in pairs if qualifies(pair[0][1], pair[1][1], holds(P, e['key']))), None)
            violated = q is not None and not arrived_before(arr, q[1][0][0])
            c['violated'] += violated
            c['undetermined'] += q is None and broken
            c['via_relay'] += q is not None and q[0] != e['writer']
            if listing is None:
                state = 'unlisted'
            else:
                state = final_state(entry_of(listing, e), e)
                if state == 'absent':
                    state = absent_state(q is not None, broken)
            c[state] += 1
            label = f'{e["writer"]} {e["key"]} -> {R}: t_write {fmt(e["t_write"])}'
            if state == 'stalled_entries':
                stalled_lines.append(f'STALLED {label}; absent, no qualifying session')
            c['late'] += violated and state == 'readable'
            in_cut = cut and cut['start_us'] <= e['t_write'] < cut['end_us']
            rec['cut_expected'] += bool(in_cut)
            if violated or state in DEFECTS:
                tag = '+'.join(t for t, on in (('VIOLATED', violated), (state.upper().replace('_', ' '), state in DEFECTS)) if on)
                if violated and state == 'readable':
                    tag += '+LATE'
                where = sides(q[0], R, q[1]) if q is not None else 'no qualifying session'
                if state == 'uncounted':
                    where += f'; verdict {entry_of(listing, e).get("verdict")}'
                defects.append(f'{tag} {label}; {where}')
            if state != 'readable':
                continue
            if arr is None:
                c['no_latency'] += 1
                continue
            ins = rrecs[arr]
            t_ready = ins['t'] if ins.get('content_status') == 'complete' else next(
                (x['t'] for x in rrecs[arr:] if x['kind'] == 'content_ready' and x['store'] == RECORDS and x['hash'] == e['hash']), None)
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
                sender = name_of.get(ins.get('from'))
                nxt = next((i for i, _ in finished(rrecs, RECORDS, ins.get('from')) if i > arr), None)
                carrier = next((pair for P, pair in pairs if P == sender and pair[0][0] == nxt), None)
                if carrier is None:
                    c['clock_unchecked'] += 1
                elif not in_clock_window(ins['t'] - off, e['t_write'], carrier[1][1]['finished'], bound):
                    clock_bad[R] = clock_bad.get(R, 0) + 1

        expected = {(e['key'], e['hash']) for e in exp}
        for item in listing or []:
            if (item['key'], item['hash']) not in expected:
                c['extra'] += 1
                defects.append(f'EXTRA {item["key"]} by {item.get("author")} -> {R}: held, and no journal has it')

    removed = meta.get('removed')
    if removed:
        K = removed['name']
        krecs, _ = recs_of(K)
        if not any(r['kind'] == 'closed' and r['store'] == RECORDS for r in krecs):
            notes.append(f'{K}\'s record-store stream never closed: it did not fold its own departure by the end of the run')
        for i, ins in enumerate(krecs):
            if ins['kind'] != 'insert_remote' or ins['store'] != RECORDS:
                continue
            P = name_of.get(ins.get('from'))
            nxt = next((x for j, x in finished(krecs, RECORDS, ins.get('from')) if j > i), None)
            pside = next((p for _, p in finished(recs_of(P)[0], RECORDS, nodes[K]['node'])
                          if p.get('session') == nxt.get('session')), None) if P and nxt else None
            if pside is None:
                continue
            remove = held_at(recs_of(P)[0], MEMBERSHIP, removed['key'])
            if served_after_remove(remove and remove[1], pside):
                c['extra'] += 1
                defects.append(f'EXTRA {ins["key"]} -> {K}: served by {P}, which held the removal at {fmt(remove[1])} '
                               f'before its side started at {fmt(pside["started"])}')

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
    if c['stalled_entries'] or c['stalled_readers']:
        reasons.append(f'stalled entries {c["stalled_entries"]}, stalled readers {c["stalled_readers"]}')
    if undetermined_share > UNDETERMINED_LIMIT:
        reasons.append(f'undetermined {undetermined_share:.1%} of expected')
    if c['unlisted']:
        reasons.append(f'unlisted entries {c["unlisted"]}')
    if background:
        reasons.append('the app left the foreground')
    verdict = 'FAIL' if found else 'INCONCLUSIVE' if reasons else 'PASS'

    head = {'FAIL': ', '.join(f'{v} {k.replace("_", " ")}' for k, v in found.items()), 'INCONCLUSIVE': ', '.join(reasons), 'PASS': ''}[verdict]
    out = [f'VERDICT: {verdict}' + (f' — {head}' if head else ''), f'run {run}']
    out.append(f'expected {c["expected"]:,} · readable {c["readable"]:,} · stalled {c["stalled_entries"]} '
               f'(readers {c["stalled_readers"]}) · split sessions {c["split"]} · undetermined {c["undetermined"]} · uncounted {c["uncounted"]}')
    out.append(f'violated {c["violated"]} · late after violation {c["late"]} · lost {c["lost"]} · content lost {c["content_lost"]} · '
               f'corrupt {c["corrupt"]} · extra {c["extra"]} · refused writes {c["refused"]} · teardown splits {c["teardown_split"]} · '
               f'unlisted {c["unlisted"]} · undetermined absent {c["undetermined_absent"]} · first qualified through a relay {c["via_relay"]}')
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
        for R, rec in recovery.items():
            first = None if rec['first'] is None else rec['first'] / 1e6
            catch = (max(rec['cut_ready']) - cut['end_us']) / 1e6 if rec['cut_ready'] and len(rec['cut_ready']) == rec['cut_expected'] else None
            conv = (max(rec['ready']) - last_write) / 1e6 if rec['ready'] else None
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
        notes.append(f'{c["clock_unchecked"]} arrivals not clock-checked: no trusted session with their sender follows them')
    if c['undetermined_absent']:
        notes.append(f'{c["undetermined_absent"]} absent entries with lost events are not called lost: lost needs a known qualifying session')
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
