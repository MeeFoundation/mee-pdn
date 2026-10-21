#!/usr/bin/env -S python3 -I
"""Checks classify.py. --synthetic builds one small run directory per state and finding in a temporary directory,
asserts the classifier's exact counts and verdict, then breaks each case's rule and asserts the case then fails.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import importlib.util
import json
import subprocess
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location('classify', HERE / 'classify.py')
C = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(C)

T0 = 1_790_000_000_000_000
PHASES = [{'name': 'steady', 'start_us': T0, 'end_us': T0 + 40_000_000},
          {'name': 'partition', 'start_us': T0 + 40_000_000, 'end_us': T0 + 70_000_000},
          {'name': 'recovery', 'start_us': T0 + 70_000_000, 'end_us': T0 + 100_000_000}]
REMOVE = 'k/remove'


def us(s):
    return T0 + round(s * 1_000_000)


def key(w, n):
    return f'by/{w}/immutable-document/{n:032x}/1'


def h(k):
    return 'h:' + k


def payload(w, k, bad=False):
    fields = {'run': 'syn', 'writer': w, 'key': k, 'pad': 'x'}
    fields['sha256'] = '0' * 64 if bad else C.payload_sha(fields)
    return json.dumps(fields)


def write(w, k, t, result='ok'):
    return {'writer': w, 'seq': 1, 'kind': 'record', 'key': k, 'hash': h(k), 'sha256': '', 'result': result, 't_write': us(t)}


def local(w, k, t, store=C.RECORDS):
    return (w, {'kind': 'insert_local', 'store': store, 'key': k, 'entry_ts': us(t), 't': us(t) + 500, 'hash': h(k), 'len': 1})


def ins(node, k, t, frm, store=C.RECORDS):
    return (node, {'kind': 'insert_remote', 'store': store, 'key': k, 't': us(t), 'entry_ts': 0, 'hash': h(k), 'len': 1,
                   'from': 'n-' + frm, 'content_status': 'incomplete'})


def ready(node, k, t):
    return (node, {'kind': 'content_ready', 'store': C.RECORDS, 'hash': h(k), 't': us(t)})


def fin(node, peer, origin, started, finished, ok=True, kind=None, sent=None, received=None, error=None, sid=None, store=C.RECORDS):
    return (node, {'kind': 'sync_finished', 'store': store, 'peer': 'n-' + peer, 'origin': origin, 'session': sid,
                   'started': us(started), 'finished': us(finished), 'ok': ok, 'error_kind': kind,
                   'error': error or (None if ok else f'{kind} failed'), 'sent': sent, 'received': received, 't': us(finished)})


def session(p, r, start, keys=(), pok=True, rok=True, rkind=None, sent=None, received=None, arrive=True, skew=0.0):
    """One reader-dialed record-store session: the serving side, then the reader's arrivals and its own side."""
    n = len(keys)
    sid = f'{p}-{r}-{start:.3f}'
    ev = [fin(p, r, 'accept', start, start + 0.1, ok=pok, sent=n if sent is None else sent, sid=sid)]
    if arrive:
        ev += [ins(r, k, start + 0.05 + skew, p) for k in keys] + [ready(r, k, start + 0.06 + skew) for k in keys]
    ev.append(fin(r, p, 'connect', start - 0.01 + skew, start + 0.2 + skew, ok=rok, kind=rkind, sid=sid,
                  received=(n if rok or rkind == 'close' else None) if received is None else received))
    return ev


def holding(keys, bad=(), missing=(), verdict='counted'):
    return [{'key': k, 'author': 'a', 'hash': h(k), 'len': 1, 'present': k not in missing, 'verdict': verdict,
             'payload': None if k in missing else payload(k.split('/')[1], k, k in bad)} for k in keys]


def build(root, name, journal, events, listing, readers=None, clock=(), cut=None, removed=None, extra_nodes=()):
    """Writes a run directory; events are (node, record) in stream order, numbered per node.
    A record of kind `_skip` drops one seq, `_resub` starts a new subscription."""
    run = root / name
    (run / 'events').mkdir(parents=True)
    (run / 'listing').mkdir()
    readers = readers or {'r': {'pass_interval_s': 5}}
    names = {j['writer'] for j in journal} | set(readers) | set(extra_nodes) | {n for n, _ in events}
    meta = {'run_id': name, 't0_us': T0, 'nodes': {n: {'node': 'n-' + n} for n in names}, 'readers': readers, 'phases': PHASES}
    if cut:
        meta['cut'] = {'start_us': us(cut[0]), 'end_us': us(cut[1])}
    if removed:
        meta['removed'] = {'name': removed, 'key': REMOVE}
    (run / 'meta.json').write_text(json.dumps(meta))
    (run / 'journal.jsonl').write_text(''.join(json.dumps(j) + '\n' for j in journal))
    (run / 'clock.jsonl').write_text(''.join(json.dumps(s) + '\n' for s in clock))
    numbering, lines = {}, {}
    for node, rec in events:
        sub, seq = numbering.get(node, (1, 0))
        if rec['kind'] == '_skip':
            numbering[node] = (sub, seq + 1)
            continue
        if rec['kind'] == '_resub':
            numbering[node] = (sub + 1, 0)
            continue
        lines.setdefault(node, []).append({**rec, 'sub': f's{sub}', 'seq': seq, 'pod': 'p'})
        numbering[node] = (sub, seq + 1)
    for node, recs in lines.items():
        (run / 'events' / f'{node}.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in recs))
    for reader, held in listing.items():
        (run / 'listing' / f'{reader}.json').write_text(json.dumps(held))
    return run


def skip(node):
    return (node, {'kind': '_skip'})


def resub(node):
    return (node, {'kind': '_resub'})


K1, K2, K3, K4 = (key('w1', i) for i in range(1, 5))


def written(w, k, t):
    """A journaled write and its author's own insert."""
    return [write(w, k, t)], [local(w, k, t)]


def cases(root):
    """Yields (name, run dir, expected counts, verdict, extra check, mutation)."""
    k417 = key('w3', 417)
    j417, ev417 = written('w3', k417, 41.203)
    before = [fin('w3', 'phone', 'accept', 38.0, 38.1, sent=0, sid='a'),
              fin('phone', 'w3', 'connect', 38.1, 38.25, received=0, sid='a')]
    qual_417 = [fin('w3', 'phone', 'accept', 43.900, 44.050, sent=1, sid='b'), ins('phone', k417, 44.160, 'w3'),
                fin('phone', 'w3', 'connect', 44.020, 44.185, received=1, sid='b'), ready('phone', k417, 44.240)]
    phone = {'phone': {'pass_interval_s': 30}}
    clock417 = [{'reader': 'phone', 'offset_us': 130_000, 'bound_us': 4_000, 't': us(44.0)}]

    def check_417(res):
        assert res['latency'] == {('partition', 'phone'): [2.907]}, res['latency']
        assert not res['clock_invalid'], res['clock_invalid']
        first, catch, _ = res['recovery']['phone']
        assert (round(first, 3), round(catch, 3)) == (0.9, 1.11), res['recovery']

    yield ('417 from its author', build(root, '417', j417, ev417 + before + qual_417, {'phone': holding([k417])},
                                        phone, clock417, cut=(40.0, 43.0)),
           dict(expected=1, readable=1), 'PASS', check_417,
           ('qualifies', lambda f: lambda r, p, held: r['ok'] and p['ok']))

    relay = [ins('w5', k417, 41.900, 'w3'), fin('w5', 'phone', 'accept', 43.000, 43.100, sent=1, sid='c'),
             ins('phone', k417, 43.050, 'w5'), ready('phone', k417, 43.060),
             fin('phone', 'w5', 'connect', 42.990, 43.200, received=1, sid='c')]
    yield ('417 through a relay', build(root, '417-relay', j417, ev417 + relay, {'phone': holding([k417])}, phone),
           dict(expected=1, readable=1, via_relay=1), 'PASS', None,
           ('held_at', lambda f: lambda recs, store, k: (lambda x: x if x and recs[x[0]]['kind'] == 'insert_local' else None)(f(recs, store, k))))

    early_relay = [fin('w5', 'phone', 'accept', 41.500, 41.600, sent=0, sid='c'),
                   fin('phone', 'w5', 'connect', 41.490, 41.700, received=0, sid='c'), ins('w5', k417, 41.900, 'w3')]
    yield ('a relay session started before the relay held it', build(root, '417-early-relay', j417,
                                                                      ev417 + early_relay + qual_417, {'phone': holding([k417])}, phone),
           dict(expected=1, readable=1), 'PASS', None,
           ('qualifies', lambda f: lambda r, p, held: r['ok'] and p['ok']))

    late_417 = [fin('w3', 'phone', 'accept', 43.900, 44.050, sent=0, sid='b'),
                fin('phone', 'w3', 'connect', 44.020, 44.185, received=0, sid='b'),
                fin('w3', 'phone', 'accept', 49.0, 49.1, sent=1, sid='d'), ins('phone', k417, 49.16, 'w3'),
                fin('phone', 'w3', 'connect', 49.12, 49.3, received=1, sid='d'), ready('phone', k417, 49.31)]

    def check_line(res):
        want = (f'VIOLATED+LATE w3 {k417} -> phone: t_write 41.203; '
                'w3 accepted 43.900-44.050 ok, phone dialed 44.020-44.185 ok')
        assert want in res['report'], res['report']

    yield ('417 violated, then late', build(root, '417-late', j417, ev417 + before + late_417, {'phone': holding([k417])}, phone, clock417),
           dict(expected=1, readable=1, violated=1, late=1), 'FAIL', check_line,
           ('arrived_before', lambda f: lambda a, p: True))

    yield ('417 lost', build(root, '417-lost', j417, ev417 + before + late_417[:2], {'phone': []}, phone, clock417),
           dict(expected=1, violated=1, lost=1, stalled_readers=0), 'FAIL', None,
           ('absent_state', lambda f: lambda q, b: 'stalled_entries'))

    def clock_case(name, off):
        j, ev = written('w1', K1, 10.0)
        ev += [fin('w1', 'r', 'accept', 10.5, 10.6, sent=1, sid='a'), ins('r', K1, 10.55, 'w1'),
               ready('r', K1, 10.56), fin('r', 'w1', 'connect', 10.49, 10.7, received=1, sid='a')]
        sample = [{'reader': 'r', 'offset_us': off, 'bound_us': 1_000, 't': us(10.0)}]
        return (name, build(root, name, j, ev, {'r': holding([K1])}, clock=sample),
                dict(expected=1, readable=1), 'PASS', lambda res: assert_eq(res['clock_invalid'], {'r'}),
                ('in_clock_window', lambda f: lambda *a: True))

    yield clock_case('clock +2 s', 2_000_000)
    yield clock_case('clock -2 s', -2_000_000)

    def stall_case(name, start, stalled):
        j, ev = written('w1', K1, 10.0)
        return (name, build(root, name, j, ev + session('w1', 'r', start, [K1]), {'r': holding([K1])}),
                dict(expected=1, readable=1, stalled_readers=int(stalled)), 'INCONCLUSIVE' if stalled else 'PASS', None,
                ('deadline', (lambda f: lambda t, i: float('inf')) if stalled else (lambda f: lambda t, i: t)))

    yield stall_case('session at 4.9 pod intervals: not stalled', 34.5, False)
    yield stall_case('session at 5.2 pod intervals: stalled', 36.0, True)

    j, ev = written('w1', K1, 10.0)
    yield ('partition never lifted', build(root, 'never-lifted', j, ev + session('w1', 'r', 5.0), {'r': []}),
           dict(expected=1, stalled_entries=1, stalled_readers=1), 'INCONCLUSIVE', None,
           ('absent_state', lambda f: lambda q, b: 'lost'))

    def gap_case(name, marker):
        j = [write('w1', K1, 1.0), write('w1', K2, 6.0)]
        ev = [local('w1', K1, 1.0), local('w1', K2, 6.0)] + session('w1', 'r', 2.0, [K1]) + [marker] + \
            session('w1', 'r', 7.0, [K2], arrive=False)
        return (name, build(root, name, j, ev, {'r': holding([K1, K2])}),
                dict(expected=2, readable=2, undetermined=1), 'INCONCLUSIVE', None,
                ('is_gap', lambda f: lambda p, r: False))

    yield gap_case('seq gap', skip('r'))
    yield gap_case('new sub', resub('r'))

    # A refusal before the pair registers leaves only the reader's side; a timeout mid-exchange only the server's.
    refused = fin('r', 'w1', 'connect', 1.5, 1.6, ok=False, kind='remote_abort', error='Remote peer aborted sync: AlreadySyncing', sid='x')
    timed_out = fin('w1', 'r', 'accept', 8.0, 9.0, ok=False, kind='sync', error='timeout', sid='y')
    for name, ev in (('reader-only, then server-only side', [refused] + session('w1', 'r', 3.0, [K1]) + [timed_out]),
                     ('server-only side before a pair', [fin('w1', 'r', 'accept', 1.5, 1.6, ok=False, kind='sync',
                                                             error='timeout', sid='y')] + session('w1', 'r', 3.0, [K1]))):
        yield (name, build(root, name.replace(' ', '-').replace(',', ''), [write('w1', K1, 1.0)], [local('w1', K1, 1.0)] + ev,
                           {'r': holding([K1])}),
               dict(expected=1, readable=1), 'PASS', None,
               ('pair_sides', lambda f: lambda rs, ps: list(zip(rs, ps))))

    ev = [local('w1', K1, 1.0)] + session('w1', 'r', 2.0, [K1], arrive=False, received=0, sent=1) + session('w1', 'r', 7.0, [K1])
    yield ('sent and received mismatch', build(root, 'mismatch', [write('w1', K1, 1.0)], ev, {'r': holding([K1])}),
           dict(expected=1, readable=1, undetermined=1), 'INCONCLUSIVE', None,
           ('counts_match', lambda f: lambda r, p: True))

    ev = [local('w1', K1, 1.0)] + session('w1', 'r', 2.0, [K1], rok=False, rkind='close') + session('w1', 'r', 7.0, [])
    yield ('teardown split', build(root, 'teardown', [write('w1', K1, 1.0)], ev, {'r': holding([K1])}),
           dict(expected=1, readable=1, teardown_split=1), 'PASS', None,
           ('is_teardown', lambda f: lambda r, p: False))

    ev = [local('w1', K1, 1.0)] + session('w1', 'r', 2.0, [K1], rok=False, rkind='sync', arrive=False) + session('w1', 'r', 7.0, [K1])
    yield ('split session', build(root, 'split', [write('w1', K1, 1.0)], ev, {'r': holding([K1])}),
           dict(expected=1, readable=1, split=1), 'FAIL', None,
           ('is_teardown', lambda f: lambda r, p: True))

    ops = [f'by/w1/mergeable-document/{1:032x}/{w}.a.1.{i}' for w, i in (('w1', 1), ('w2', 1), ('w2', 2))]
    jo = [{**write(w, k, 1.0 + i), 'kind': 'op'} for i, (w, k) in enumerate(zip(('w1', 'w2', 'w2'), ops))]
    ev = [local(j['writer'], j['key'], 1.0 + i) for i, j in enumerate(jo)] + \
        [ins('w1', k, 4.0, 'w2') for k in ops[1:]] + session('w1', 'r', 5.0, ops)
    yield ('every operation is expected, another writer\'s through the document\'s member', build(root, 'ops', jo, ev, {'r': holding(ops)}),
           dict(expected=3, readable=3, via_relay=2), 'PASS', None,
           ('expected_writes', lambda f: lambda journal: f(journal)[:1]))

    ev = [local('w1', K1, 1.0)] + session('w1', 'r', 2.0, [K1])
    yield ('refused write excluded', build(root, 'refused', [write('w1', K1, 1.0), write('w1', K2, 1.5, result='refused')],
                                           ev, {'r': holding([K1])}),
           dict(expected=1, readable=1, refused=1), 'PASS', None,
           ('expected_writes', lambda f: lambda journal: f([{**j, 'result': 'ok'} for j in journal])))

    removed_base = [local('w1', REMOVE, 20.0, store=C.MEMBERSHIP), local('w1', K3, 24.0)]
    leak = [fin('w1', 'k', 'accept', 25.0, 25.1, sent=1, sid='k1'), ins('k', K3, 25.05, 'w1'),
            fin('k', 'w1', 'connect', 24.99, 25.2, received=1, sid='k1')]
    yield ('a member that held the removal serves the removaled member', build(root, 'remove-leak', [write('w1', K3, 24.0)],
                                                                         removed_base + leak + session('w1', 'r', 30.0, [K3]),
                                                                         {'r': holding([K3])}, removed='k'),
           dict(expected=1, readable=1, extra=1), 'FAIL', None,
           ('served_after_remove', lambda f: lambda held, p: False))

    race = [local('w2', K4, 19.0), fin('w2', 'k', 'accept', 21.0, 21.1, sent=1, sid='k2'), ins('k', K4, 21.05, 'w2'),
            fin('k', 'w2', 'connect', 20.99, 21.2, received=1, sid='k2'), ins('w2', REMOVE, 22.0, 'w1', store=C.MEMBERSHIP),
            (('k'), {'kind': 'closed', 'store': C.RECORDS, 't': us(23.0)})]
    yield ('a member that had not folded the removal serves the removaled member', build(root, 'remove-race', [write('w2', K4, 19.0)],
                                                                                   [local('w1', REMOVE, 20.0, store=C.MEMBERSHIP)]
                                                                                   + race + session('w2', 'r', 30.0, [K4]),
                                                                                   {'r': holding([K4])}, removed='k'),
           dict(expected=1, readable=1), 'PASS', None,
           ('served_after_remove', lambda f: lambda held, p: True))

    ev = [local('w1', K1, 1.0)] + session('w1', 'r', 2.0, [K1])
    yield ('corrupt', build(root, 'corrupt', [write('w1', K1, 1.0)], ev, {'r': holding([K1], bad=[K1])}),
           dict(expected=1, corrupt=1), 'FAIL', None,
           ('payload_ok', lambda f: lambda text: True))

    yield ('content lost', build(root, 'content-lost', [write('w1', K1, 1.0)], ev, {'r': holding([K1], missing=[K1])}),
           dict(expected=1, content_lost=1), 'FAIL', None,
           ('final_state', lambda f: lambda *a: {'content_lost': 'readable'}.get(f(*a), f(*a))))

    yield ('uncounted', build(root, 'uncounted', [write('w1', K1, 1.0)], ev, {'r': holding([K1], verdict='not_yet')}),
           dict(expected=1, uncounted=1), 'FAIL', None,
           ('final_state', lambda f: lambda *a: {'uncounted': 'readable'}.get(f(*a), f(*a))))

    yield ('an entry no journal has', build(root, 'extra', [write('w1', K1, 1.0)], ev, {'r': holding([K1, K2])}),
           dict(expected=1, readable=1, extra=1), 'FAIL', None,
           ('expected_writes', lambda f: lambda journal: f(journal) + [write('w1', K2, 1.0)]))

    forged = holding([K1]) + [{**holding([K1])[0], 'author': 'f', 'hash': 'h:forged'}]
    yield ('a second entry at a written key, by another author', build(root, 'forged', [write('w1', K1, 1.0)], ev, {'r': forged}),
           dict(expected=1, readable=1, extra=1), 'FAIL', None,
           ('entry_of', lambda f: lambda items, e: next(i for i in items if i['key'] == e['key'] and i['author'] == 'f')))

    keys = [key('w1', i) for i in range(1, 11)]
    ev = [local('w1', k, 1.0) for k in keys] + [fin('w1', 'r', 'accept', 1.5, 1.6, sent=10, sid='a')] + \
        [ins('r', k, 1.55, 'w1') for k in keys] + [fin('r', 'w1', 'connect', 1.49, 1.7, received=10, sid='a')] + \
        [ready('r', k, 1.0 + i) for i, k in enumerate(keys, 1)]
    yield ('nearest-rank percentiles', build(root, 'percentiles', [write('w1', k, 1.0) for k in keys], ev, {'r': holding(keys)}),
           dict(expected=10, readable=10), 'PASS',
           lambda res: assert_eq([C.nearest_rank(sorted(res['latency'][('steady', 'r')]), p) for p in (50, 90, 99)], [5.0, 9.0, 10.0]),
           ('nearest_rank', lambda f: lambda v, p: v[min(len(v) - 1, int(p / 100 * len(v)))]))


def assert_eq(got, want):
    assert got == want, f'{got} != {want}'


def evaluate(run, counts, verdict, extra):
    res = C.classify(run)
    want = dict.fromkeys(C.COUNTS, 0) | counts
    got = {k: v for k, v in res['counts'].items() if k not in ('no_latency', 'clock_unchecked')}
    want = {k: v for k, v in want.items() if k in got}
    assert got == want, f'counts {({k: (got[k], want[k]) for k in got if got[k] != want[k]})}'
    assert res['verdict'] == verdict, f'verdict {res["verdict"]} != {verdict}'
    if extra:
        extra(res)


def synthetic(keep):
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        root = keep or Path(tmp)
        all_cases = list(cases(root))
        for name, run, counts, verdict, extra, (attr, breaker) in all_cases:
            try:
                evaluate(run, counts, verdict, extra)
                status = 'ok'
            except AssertionError as err:
                failures += 1
                status = f'FAILED: {err}'
            original = getattr(C, attr)
            setattr(C, attr, breaker(original))
            try:
                evaluate(run, counts, verdict, extra)
                bite = 'DOES NOT BITE'
                failures += 1
            except AssertionError as err:
                bite = f'bites ({type(err).__name__}: {str(err)[:70]})'
            finally:
                setattr(C, attr, original)
            print(f'{name}: {status}; broken {attr}: {bite}')
        csv_path = root / 'runs.csv'
        for name, run, _, verdict, *_ in all_cases[:3]:
            code = subprocess.run([sys.executable, '-I', str(HERE / 'classify.py'), str(run), '--runs-csv', str(csv_path)],
                                  capture_output=True, text=True).returncode
            report = (run / 'report.txt').read_text()
            if code != C.EXIT[verdict] or not report.startswith(f'VERDICT: {verdict}'):
                failures += 1
                print(f'CLI {name}: exit {code}, report {report.splitlines()[0]!r}')
        rows = csv_path.read_text().splitlines()
        if len(rows) != 4:
            failures += 1
            print(f'runs.csv has {len(rows)} lines, want a header and 3 rows')
    print('synthetic: ' + ('PASS' if not failures else f'{failures} FAILED'))
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--synthetic', action='store_true', required=True)
    ap.add_argument('--keep', type=Path, help='build the run directories here, left in place with their reports')
    args = ap.parse_args()
    return synthetic(args.keep)


if __name__ == '__main__':
    sys.exit(main())
