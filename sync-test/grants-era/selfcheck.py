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


def us(s):
    return T0 + round(s * 1_000_000)


def h(w, path, version=0):
    return f'{w}:{path}:{version}'


def payload(w, path, bad=False):
    fields = {'run': 'syn', 'writer': w, 'path': path, 'pad': 'x'}
    fields['sha256'] = '0' * 64 if bad else C.payload_sha(fields)
    return json.dumps(fields)


def write(w, path, t, version=None, result='ok'):
    j = {'writer': w, 'seq': int(path[2:]), 'path': path, 'sha256': '', 'result': result,
         't_write': us(t), 'hash': h(w, path, version or 0)}
    if version is not None:
        j['version'] = version
    return j


def ins(w, path, t, version=0):
    return {'kind': 'insert_remote', 'issuer': 'i-' + w, 'path': path, 't': us(t), 'hash': h(w, path, version),
            'len': 1, 'from': 'n-' + w, 'content_status': 'incomplete'}


def ready(w, path, t, version=0):
    return {'kind': 'content_ready', 'issuer': 'i-' + w, 'hash': h(w, path, version), 't': us(t)}


def fin(w, peer, origin, started, finished, ok=True, kind=None, sent=None, received=None, error=None, sid=None):
    return {'kind': 'sync_finished', 'issuer': 'i-' + w, 'peer': 'n-' + peer, 'origin': origin, 'session': sid,
            'started': us(started), 'finished': us(finished), 'ok': ok, 'error_kind': kind,
            'error': error or (None if ok else f'{kind} failed'), 'sent': sent, 'received': received, 't': us(finished)}


def session(w, r, start, paths=(), wok=True, rok=True, rkind=None, sent=None, received=None, arrive=True, skew=0.0):
    """One reader-dialed session: the writer's accepted side, then the reader's arrivals and its dialed side."""
    n = len(paths)
    ev = [(w, fin(w, r, 'accept', start, start + 0.1, ok=wok, sent=n if sent is None else sent, sid=f'{start:.3f}'))]
    if arrive:
        ev += [(r, ins(w, p, start + 0.05 + skew)) for p in paths] + [(r, ready(w, p, start + 0.06 + skew)) for p in paths]
    ev.append((r, fin(w, w, 'connect', start - 0.01 + skew, start + 0.2 + skew, ok=rok, kind=rkind, sid=f'{start:.3f}',
                      received=(n if rok or rkind == 'close' else None) if received is None else received)))
    return ev


def holding(w, paths, bad=(), missing=()):
    return {'i-' + w: {p: {'hash': h(w, p, v), 'len': 1, 'present': p not in missing,
                           'payload': None if p in missing else payload(w, p, p in bad)} for p, v in paths}}


def build(root, name, journal, events, listing, readers=None, clock=(), cut=None):
    """Writes a run directory; events are (node, record) in stream order, numbered per node and issuer.
    A record of kind `_skip` drops one seq, `_resub` starts a new subscription."""
    run = root / name
    (run / 'events').mkdir(parents=True)
    (run / 'listing').mkdir()
    writers = sorted({j['writer'] for j in journal})
    readers = readers or {'r': {'interval_s': 5, 'covers': {w: 'all' for w in writers}}}
    names = set(writers) | set(readers)
    meta = {'run_id': name, 't0_us': T0, 'nodes': {n: {'node': 'n-' + n, 'issuer': 'i-' + n} for n in names},
            'readers': readers, 'phases': PHASES}
    if cut:
        meta['cut'] = {'start_us': us(cut[0]), 'end_us': us(cut[1])}
    (run / 'meta.json').write_text(json.dumps(meta))
    (run / 'journal.jsonl').write_text(''.join(json.dumps(j) + '\n' for j in journal))
    (run / 'clock.jsonl').write_text(''.join(json.dumps(s) + '\n' for s in clock))
    numbering, lines = {}, {}
    for node, rec in events:
        key = (node, rec.get('issuer', 'i-' + (writers[0] if writers else '')))
        sub, seq = numbering.get(key, (1, 0))
        if rec['kind'] == '_skip':
            numbering[key] = (sub, seq + 1)
            continue
        if rec['kind'] == '_resub':
            numbering[key] = (sub + 1, 0)
            continue
        lines.setdefault(node, []).append({**rec, 'sub': f's{sub}', 'seq': seq, 'namespace': 'ns-' + rec['issuer']})
        numbering[key] = (sub, seq + 1)
    for node, recs in lines.items():
        (run / 'events' / f'{node}.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in recs))
    for reader, held in listing.items():
        (run / 'listing' / f'{reader}.json').write_text(json.dumps(held))
    return run


def skip(w):
    return {'kind': '_skip', 'issuer': 'i-' + w}


def resub(w):
    return {'kind': '_resub', 'issuer': 'i-' + w}


S1, S2, S3, S4 = (f's/00000000{i}' for i in range(1, 5))
H1 = 'h/000001'


def cases(root):
    """Yields (name, run dir, expected counts, verdict, extra check, mutation)."""
    p417 = 's/000000417'
    j417 = [write('w3', p417, 41.203)]
    before = [('w3', fin('w3', 'phone', 'accept', 38.0, 38.1, sent=0, sid='a')),
              ('phone', fin('w3', 'w3', 'connect', 38.1, 38.25, received=0, sid='a'))]
    qual_417 = [('w3', fin('w3', 'phone', 'accept', 43.900, 44.050, sent=1, sid='b')),
                ('phone', ins('w3', p417, 44.160)), ('phone', fin('w3', 'w3', 'connect', 44.020, 44.185, received=1, sid='b')),
                ('phone', ready('w3', p417, 44.240))]
    phone = {'phone': {'interval_s': 5, 'covers': {'w3': 'all'}}}
    clock417 = [{'reader': 'phone', 'offset_us': 130_000, 'bound_us': 4_000, 't': us(44.0)}]

    def check_417(res):
        assert res['latency'] == {('partition', 'phone'): [2.907]}, res['latency']
        assert not res['clock_invalid'], res['clock_invalid']
        first, catch, _ = res['recovery']['phone']
        assert (round(first, 3), round(catch, 3)) == (0.9, 1.11), res['recovery']

    yield ('417 in flight, then delivered', build(root, '417', j417, before + qual_417, {'phone': holding('w3', [(p417, 0)])},
                                                    phone, clock417, cut=(40.0, 43.0)),
           dict(expected=1, readable=1), 'PASS', check_417,
           ('qualifies', lambda f: lambda r, w, t: r['ok'] and w['ok']))

    late_417 = [('w3', fin('w3', 'phone', 'accept', 43.900, 44.050, sent=0, sid='b')),
                ('phone', fin('w3', 'w3', 'connect', 44.020, 44.185, received=0, sid='b'))] + [
               ('w3', fin('w3', 'phone', 'accept', 49.0, 49.1, sent=1, sid='c')),
               ('phone', ins('w3', p417, 49.16)), ('phone', fin('w3', 'w3', 'connect', 49.12, 49.3, received=1, sid='c')),
               ('phone', ready('w3', p417, 49.31))]

    def check_line(res):
        want = 'VIOLATED+LATE w3 s/000000417 -> phone: t_write 41.203; w3 accepted 43.900-44.050 ok, phone dialed 44.020-44.185 ok'
        assert want in res['report'], res['report']

    yield ('417 violated, then late', build(root, '417-late', j417, before + late_417, {'phone': holding('w3', [(p417, 0)])}, phone, clock417),
           dict(expected=1, readable=1, violated=1, late=1), 'FAIL', check_line,
           ('arrived_before', lambda f: lambda a, p: True))

    yield ('417 lost', build(root, '417-lost', j417, before + late_417[:2], {'phone': {}}, phone, clock417),
           dict(expected=1, violated=1, lost=1), 'FAIL', None,
           ('absent_state', lambda f: lambda q, b: 'stalled_entries'))

    def clock_case(name, off):
        j = [write('w1', S1, 10.0)]
        ev = [('w1', fin('w1', 'r', 'accept', 10.5, 10.6, sent=1, sid='a')), ('r', ins('w1', S1, 10.55)),
              ('r', ready('w1', S1, 10.56)), ('r', fin('w1', 'w1', 'connect', 10.49, 10.7, received=1, sid='a'))]
        sample = [{'reader': 'r', 'offset_us': off, 'bound_us': 1_000, 't': us(10.0)}]
        return (name, build(root, name, j, ev, {'r': holding('w1', [(S1, 0)])}, clock=sample),
                dict(expected=1, readable=1), 'PASS', lambda res: assert_eq(res['clock_invalid'], {'r'}),
                ('in_clock_window', lambda f: lambda *a: True))

    yield clock_case('clock +2 s', 2_000_000)
    yield clock_case('clock -2 s', -2_000_000)

    def stall_case(name, start, stalled):
        j = [write('w1', S1, 10.0)]
        return (name, build(root, name, j, session('w1', 'r', start, [S1]), {'r': holding('w1', [(S1, 0)])}),
                dict(expected=1, readable=1, stalled_pairs=int(stalled)), 'INCONCLUSIVE' if stalled else 'PASS', None,
                ('deadline', (lambda f: lambda t, i: float('inf')) if stalled else (lambda f: lambda t, i: t)))

    yield stall_case('session at 4.9 intervals: not stalled', 34.5, False)
    yield stall_case('session at 5.2 intervals: stalled', 36.0, True)

    yield ('partition never lifted', build(root, 'never-lifted', [write('w1', S1, 10.0)],
                                           session('w1', 'r', 5.0), {'r': {}}),
           dict(expected=1, stalled_pairs=1, stalled_entries=1), 'INCONCLUSIVE', None,
           ('absent_state', lambda f: lambda q, b: 'lost'))

    def gap_case(name, marker):
        j = [write('w1', S1, 1.0), write('w1', S2, 6.0)]
        ev = session('w1', 'r', 2.0, [S1]) + [('r', marker)] + session('w1', 'r', 7.0, [S2], arrive=False)
        return (name, build(root, name, j, ev, {'r': holding('w1', [(S1, 0), (S2, 0)])}),
                dict(expected=2, readable=2, undetermined=1), 'INCONCLUSIVE', None,
                ('is_gap', lambda f: lambda p, r: False))

    yield gap_case('seq gap', skip('w1'))
    yield gap_case('new sub', resub('w1'))

    # A refusal before the pair registers leaves only the reader's side; a timeout mid-exchange only the writer's.
    refused = ('r', fin('w1', 'w1', 'connect', 1.5, 1.6, ok=False, kind='remote_abort',
                        error='Remote peer aborted sync: AlreadySyncing', sid='x'))
    timed_out = ('w1', fin('w1', 'r', 'accept', 8.0, 9.0, ok=False, kind='sync', error='timeout', sid='y'))
    for name, ev in (('reader-only, then writer-only side', [refused] + session('w1', 'r', 3.0, [S1]) + [timed_out]),
                     ('writer-only side before a pair', [('w1', fin('w1', 'r', 'accept', 1.5, 1.6, ok=False, kind='sync',
                                                                    error='timeout', sid='y'))] + session('w1', 'r', 3.0, [S1]))):
        yield (name, build(root, name.replace(' ', '-').replace(',', ''), [write('w1', S1, 1.0)], ev, {'r': holding('w1', [(S1, 0)])}),
               dict(expected=1, readable=1), 'PASS', None,
               ('pair_sides', lambda f: lambda rs, ws: list(zip(rs, ws))))

    ev = session('w1', 'r', 2.0, [S1], arrive=False, received=0, sent=1) + session('w1', 'r', 7.0, [S1])
    yield ('sent and received mismatch', build(root, 'mismatch', [write('w1', S1, 1.0)], ev, {'r': holding('w1', [(S1, 0)])}),
           dict(expected=1, readable=1, undetermined=1), 'INCONCLUSIVE', None,
           ('counts_match', lambda f: lambda r, w: True))

    ev = session('w1', 'r', 2.0, [S1], rok=False, rkind='close') + session('w1', 'r', 7.0, [])
    yield ('teardown split', build(root, 'teardown', [write('w1', S1, 1.0)], ev, {'r': holding('w1', [(S1, 0)])}),
           dict(expected=1, readable=1, teardown_split=1), 'PASS', None,
           ('is_teardown', lambda f: lambda r, w: False))

    ev = session('w1', 'r', 2.0, [S1], rok=False, rkind='sync', arrive=False) + session('w1', 'r', 7.0, [S1])
    yield ('split session', build(root, 'split', [write('w1', S1, 1.0)], ev, {'r': holding('w1', [(S1, 0)])}),
           dict(expected=1, readable=1, split=1), 'FAIL', None,
           ('is_teardown', lambda f: lambda r, w: True))

    jh = [write('w1', H1, 1.0, 1), write('w1', H1, 2.0, 2), write('w1', H1, 3.0, 3)]
    ev = [('w1', fin('w1', 'r', 'accept', 4.0, 4.1, sent=1, sid='a')), ('r', ins('w1', H1, 4.05, 3)),
          ('r', ready('w1', H1, 4.06, 3)), ('r', fin('w1', 'w1', 'connect', 3.99, 4.2, received=1, sid='a'))]
    yield ('only the last h/ version expected', build(root, 'h-last', jh, ev, {'r': holding('w1', [(H1, 3)])}),
           dict(expected=1, readable=1), 'PASS', None,
           ('expected_writes', lambda f: lambda journal: [j for j in journal if j['result'] == 'ok']))

    ev = [('w1', fin('w1', 'r', 'accept', 4.0, 4.1, sent=1, sid='a')), ('r', ins('w1', H1, 4.05, 3)),
          ('r', fin('w1', 'w1', 'connect', 3.99, 4.2, received=1, sid='a'))]
    yield ('stale', build(root, 'stale', jh, ev, {'r': holding('w1', [(H1, 2)])}),
           dict(expected=1, stale=1), 'FAIL', None,
           ('final_state', lambda f: lambda *a: {'stale': 'readable'}.get(f(*a), f(*a))))

    yield ('refused write excluded', build(root, 'refused', [write('w1', S1, 1.0), write('w1', S2, 1.5, result='refused')],
                                           session('w1', 'r', 2.0, [S1]), {'r': holding('w1', [(S1, 0)])}),
           dict(expected=1, readable=1, refused=1), 'PASS', None,
           ('expected_writes', lambda f: lambda journal: f([{**j, 'result': 'ok'} for j in journal])))

    jp = [write('w1', p, 1.0) for p in (S1, S2, S3, S4)]
    partial = {'partial': {'interval_s': 5, 'covers': {'w1': [S2, S4]}}}
    yield ('partial holds its set', build(root, 'partial', jp, session('w1', 'partial', 2.0, [S2, S4]),
                                          {'partial': holding('w1', [(S2, 0), (S4, 0)])}, partial),
           dict(expected=2, readable=2), 'PASS', None,
           ('covered', lambda f: lambda cover, path: True))

    yield ('extra', build(root, 'extra', jp, session('w1', 'partial', 2.0, [S2, S3, S4]),
                          {'partial': holding('w1', [(S2, 0), (S3, 0), (S4, 0)])}, partial),
           dict(expected=2, readable=2, extra=1), 'FAIL', None,
           ('covered', lambda f: lambda cover, path: True))

    yield ('corrupt', build(root, 'corrupt', [write('w1', S1, 1.0)], session('w1', 'r', 2.0, [S1]),
                            {'r': holding('w1', [(S1, 0)], bad=[S1])}),
           dict(expected=1, corrupt=1), 'FAIL', None,
           ('payload_ok', lambda f: lambda text: True))

    yield ('content lost', build(root, 'content-lost', [write('w1', S1, 1.0)], session('w1', 'r', 2.0, [S1]),
                                 {'r': holding('w1', [(S1, 0)], missing=[S1])}),
           dict(expected=1, content_lost=1), 'FAIL', None,
           ('final_state', lambda f: lambda *a: {'content_lost': 'readable'}.get(f(*a), f(*a))))

    paths = [f's/{i:09d}' for i in range(1, 11)]
    ev = [('w1', fin('w1', 'r', 'accept', 1.5, 1.6, sent=10, sid='a'))] + [('r', ins('w1', p, 1.55)) for p in paths] + [
          ('r', fin('w1', 'w1', 'connect', 1.49, 1.7, received=10, sid='a'))] + [
          ('r', ready('w1', p, 1.0 + i)) for i, p in enumerate(paths, 1)]
    yield ('nearest-rank percentiles', build(root, 'percentiles', [write('w1', p, 1.0) for p in paths], ev,
                                             {'r': holding('w1', [(p, 0) for p in paths])}),
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
