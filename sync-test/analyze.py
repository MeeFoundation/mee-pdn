#!/usr/bin/env -S python3 -I
"""Analyzes load.csv: per metric, the slopes in N and V of each test, and whether the laptop is honest for the phone.

The rules are load.md's "Is the laptop honest?". A row of load.csv is one cell of one repeat: `test` (L or P),
`variant` (plain, net, slow for L; phone for P), `N`, `V`, `S`, `repeat`, the marks `contended`, `lagged` (a count),
`incomplete`, `inactive`, `low_power`, `slept`, and the warnings `not_quiet` and `thermal` (load.md "A cell's validity"), and one column per metric in METRICS. `--synthetic` checks the analysis on tables whose
verdicts are known, and that each check fails with its rule broken.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import csv
import math
import statistics
import tempfile
from pathlib import Path

# A time metric moves when processes compete for the CPU; a work metric does not; an events metric is counted
# from the event stream, so a lost event undercounts it.
METRICS = {
    'steady_p50_s': 'time', 'steady_p90_s': 'time', 'steady_p99_s': 'time',
    'catchup_s': 'time', 'catchup_records_per_s': 'time',
    'r_cpu_s_per_record': 'work', 'sessions_per_record': 'events', 'idle_r_cpu_pct': 'work',
    'idle_long_r_cpu_pct': 'work', 'r_rss_mb': 'work',
}
RATIO_CV_LIMIT = 0.25
# Two slopes are the same shape when their whole 95% interval of difference lies within this margin, and differ
# when it lies wholly outside it; between the two the run cannot tell.
SLOPE_MARGIN = 0.2
Z95 = 1.96
# Two-sided 95% quantiles of Student's t by degrees of freedom; past the table the normal one.
T95 = {1: 12.71, 2: 4.30, 3: 3.18, 4: 2.78, 5: 2.57, 6: 2.45, 7: 2.36, 8: 2.31, 9: 2.26, 10: 2.23, 11: 2.20,
       12: 2.18, 13: 2.16, 14: 2.14, 15: 2.13, 16: 2.12, 17: 2.11, 18: 2.10, 19: 2.09, 20: 2.09, 25: 2.06, 30: 2.04}


def t95(df):
    return T95.get(df) or next((T95[d] for d in sorted(T95) if d >= df), Z95)


def load(path):
    with path.open() as f:
        return list(csv.DictReader(f))


def usable(r, kind):
    """Whether a row enters the fits of a metric of this kind (load.md "A cell's validity")."""
    if any(r.get(m) == '1' for m in ('incomplete', 'join_hung', 'inactive', 'low_power', 'slept')) or r.get('not_run'):
        return False
    lagged = int(r.get('lagged') or 0) > 0
    if kind == 'time':
        return r.get('contended') != '1' and not lagged
    return not (kind == 'events' and lagged)


def cells(rows, test, variant, metric, kind):
    """Per level (N, V, S): the median over usable repeats, and the median of the records the pod actually held when
    those repeats started (`V_actual`, else the level). A level whose median is not positive is left out; `zeros`
    names it."""
    by = {}
    for r in rows:
        if r['test'] != test or r['variant'] != variant or r.get(metric) in (None, ''):
            continue
        if not usable(r, kind):
            continue
        held = float(r.get('V_actual') or r['V'])
        by.setdefault((int(r['N']), int(r['V']), int(r['S'])), []).append((float(r[metric]), held))
    out = {}
    for k, vals in by.items():
        m = statistics.median(x for x, _ in vals)
        if m > 0:
            out[k] = (m, statistics.median(v for _, v in vals))
    return out


def zeros(rows, metric):
    """Rows whose value is not positive: they enter no fit, so the report names them."""
    return [r for r in rows if r.get(metric) not in (None, '') and float(r[metric]) <= 0]


def solve3(a, b):
    """Gaussian elimination of a 3×3 system; None when singular."""
    m = [row[:] + [rhs] for row, rhs in zip(a, b)]
    for i in range(3):
        p = max(range(i, 3), key=lambda r: abs(m[r][i]))
        if abs(m[p][i]) < 1e-12:
            return None
        m[i], m[p] = m[p], m[i]
        for r in range(3):
            if r != i:
                f = m[r][i] / m[i][i]
                m[r] = [x - f * y for x, y in zip(m[r], m[i])]
    return [m[i][3] / m[i][i] for i in range(3)]


def fit(cellmap):
    """log m = a + b·log N + c·log V over the levels with records, by least squares: (b, se_b, c, se_c, n), or None
    with too few cells. The empty pod is not a point of the V slope: log(0 + 1) would sit far below the next level
    and the slope would describe the step from empty, not growth; `baseline` reports it on its own."""
    pts = [(math.log(n), math.log(held), math.log(m)) for (n, v, _), (m, held) in cellmap.items() if v > 0 and held > 0]
    if len({v for (_, v, _) in cellmap if v > 0}) < 2:
        return fit_n(pts)
    if len(pts) < 4:
        return None
    xs = [(1.0, x1, x2) for x1, x2, _ in pts]
    ys = [y for *_, y in pts]
    xtx = [[sum(x[i] * x[j] for x in xs) for j in range(3)] for i in range(3)]
    xty = [sum(x[i] * y for x, y in zip(xs, ys)) for i in range(3)]
    beta = solve3(xtx, xty)
    if beta is None:
        return None
    resid = [y - sum(bi * xi for bi, xi in zip(beta, x)) for x, y in zip(xs, ys)]
    df = len(pts) - 3
    s2 = sum(r * r for r in resid) / df if df > 0 else 0.0
    inv = [solve3(xtx, [1.0 if i == j else 0.0 for i in range(3)]) for j in range(3)]
    if any(col is None for col in inv):
        return None
    se = [math.sqrt(max(s2 * inv[i][i], 0.0)) for i in range(3)]
    return beta[1], se[1], beta[2], se[2], len(pts)


def fit_n(pts):
    """log m = a + b·log N over one level of V: (b, se_b, None, None, n), or None with too few cells. A metric
    measured at one level with records has no V slope to fit."""
    if len(pts) < 3:
        return None
    xs = [x for x, _, _ in pts]
    ys = [y for _, _, y in pts]
    mx, my = statistics.mean(xs), statistics.mean(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    if sxx == 0:
        return None
    b = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx
    resid = [y - my - b * (x - mx) for x, y in zip(xs, ys)]
    s2 = sum(r * r for r in resid) / (len(pts) - 2)
    # One more than the points: callers take the degrees of freedom as n − 3, and this fit has 2 parameters.
    return b, math.sqrt(s2 / sxx), None, None, len(pts) + 1


def slopes_agree(fl, fp):
    """'same' when both slopes' 95% intervals of difference lie within SLOPE_MARGIN, 'different' when either lies
    wholly outside it, else 'unsure'."""
    (bl, sbl, cl, scl, nl), (bp, sbp, cp, scp, np_) = fl, fp
    t = t95(min(nl, np_) - 3)
    verdicts = []
    pairs = [(abs(bl - bp), t * math.hypot(sbl, sbp))]
    if cl is not None and cp is not None:
        pairs.append((abs(cl - cp), t * math.hypot(scl, scp)))
    for d, h in pairs:
        verdicts.append('same' if d + h <= SLOPE_MARGIN else 'different' if d - h > SLOPE_MARGIN else 'unsure')
    return 'different' if 'different' in verdicts else 'unsure' if 'unsure' in verdicts else 'same'


def ratio(lcells, pcells):
    shared = sorted(k for k in set(lcells) & set(pcells) if k[1] > 0)
    return {k: pcells[k][0] / lcells[k][0] for k in shared}


def baseline(lcells, pcells):
    """The ratio at the empty pod, per N."""
    return {k: pcells[k][0] / lcells[k][0] for k in sorted(set(lcells) & set(pcells)) if k[1] == 0}


def ratio_constant(ratios):
    vals = list(ratios.values())
    if len(vals) < 2:
        return False
    return statistics.pstdev(vals) / statistics.mean(vals) <= RATIO_CV_LIMIT


def verdict(metric, rows, variant, size):
    """('honest' | 'shape only' | 'not honest' | 'too noisy' | 'no phone' | 'too few cells', detail) for one L
    variant and one size."""
    kind = METRICS[metric]
    lc = {k: v for k, v in cells(rows, 'L', variant, metric, kind).items() if k[2] == size}
    pc = {k: v for k, v in cells(rows, 'P', 'phone', metric, kind).items() if k[2] == size}
    fl = fit(lc)
    if fl is None:
        return 'too few cells', {}
    if not pc:
        return 'no phone', {'fit_l': fl}
    fp = fit(pc)
    if fp is None:
        return 'too few cells', {'fit_l': fl}
    ratios = ratio(lc, pc)
    detail = {'fit_l': fl, 'fit_p': fp, 'ratios': ratios, 'baseline': baseline(lc, pc)}
    shape = slopes_agree(fl, fp)
    if shape == 'different':
        return 'not honest', detail
    if shape == 'unsure':
        return 'too noisy', detail
    if ratio_constant(ratios):
        detail['factor'] = statistics.median(ratios.values())
        return 'honest', detail
    return 'shape only', detail


def interval(fit_, which):
    """A slope with its 95% half-width, as the report prints it."""
    slope, se = (fit_[0], fit_[1]) if which == 'N' else (fit_[2], fit_[3])
    if slope is None:
        return '-'
    return f'{slope:.2f}±{t95(fit_[4] - 3) * se:.2f}'


def unfinished(rows):
    """The cells the platform did not finish, in matrix order: the report's first table."""
    return sorted((r for r in rows if r.get('join_hung') == '1' or r.get('not_run')),
                  key=lambda r: (r['test'], r['variant'], int(r['N']), int(r['S']), int(r['V']), int(r['repeat'])))


def report(rows):
    out = []
    hung = unfinished(rows)
    out.append('cells the platform did not finish' + ('' if hung else ': none'))
    for r in hung:
        why = 'join_hung' if r.get('join_hung') == '1' else f'not_run: {r["not_run"]}'
        evidence = f' — evidence {r["evidence"]}' if r.get('evidence') else ''
        out.append(f'  {r["test"]}/{r["variant"]} N={r["N"]} V={r["V"]} S={r["S"]} repeat {r["repeat"]}: {why}{evidence}')
    sizes = sorted({int(r['S']) for r in rows})
    variants = sorted({r['variant'] for r in rows if r['test'] == 'L'})
    for size in sizes:
        out.append(f'payload {size} B')
        out.append(f'  {"metric":<24} {"L variant":<8} {"slope N (L / P)":<24} {"slope V (L / P)":<24} {"ratio P/L":<12} {"cv":<6} {"at V=0":<10} verdict')
        for metric in METRICS:
            for variant in variants:
                v, d = verdict(metric, rows, variant, size)
                if v == 'too few cells':
                    continue
                fl, fp = d.get('fit_l'), d.get('fit_p')
                sn = interval(fl, 'N') + (f' / {interval(fp, "N")}' if fp else '')
                sv = interval(fl, 'V') + (f' / {interval(fp, "V")}' if fp else '')
                base = list(d.get('baseline', {}).values())
                b0 = f'{statistics.median(base):.2f}' if base else '-' 
                rs = list(d.get('ratios', {}).values())
                rr = f'{min(rs):.2f}–{max(rs):.2f}' if rs else '-'
                cv = f'{statistics.pstdev(rs) / statistics.mean(rs):.2f}' if len(rs) > 1 else '-'
                tail = f', factor {d["factor"]:.2f}' if 'factor' in d else ''
                if v == 'not honest':
                    worst = max(d['ratios'].items(), key=lambda kv: abs(math.log(kv[1] / statistics.median(rs))))[0]
                    tail = f'; farthest cell N={worst[0]} V={worst[1]}'
                out.append(f'  {metric:<24} {variant:<8} {sn:<24} {sv:<24} {rr:<12} {cv:<6} {b0:<10} {v}{tail}')
    out.append('cells left out')
    for r in rows:
        marks = [m for m in ('incomplete', 'inactive', 'low_power', 'slept', 'contended') if r.get(m) == '1']
        marks += [f'lagged {r["lagged"]}'] if int(r.get('lagged') or 0) else []
        marks += [f'{m} is 0' for m in METRICS if r.get(m) not in (None, '') and float(r[m]) <= 0]
        if marks:
            out.append(f'  {r["test"]}/{r["variant"]} N={r["N"]} V={r["V"]} S={r["S"]} repeat {r["repeat"]}: {", ".join(marks)}')
    out.append('cells measured with a warning (kept in the fits)')
    for r in rows:
        warned = [m for m in ('not_quiet', 'thermal', 'pod_pass') if r.get(m) == '1']
        if warned:
            out.append(f'  {r["test"]}/{r["variant"]} N={r["N"]} V={r["V"]} S={r["S"]} repeat {r["repeat"]}: {", ".join(warned)}')
    observed = [r for r in rows if r['test'] == 'P' and r.get('observation_cpu_s') not in (None, '')]
    if observed:
        out.append('observation cost on the phone, CPU seconds per cell (process total less the node threads)')
        for r in observed:
            out.append(f'  N={r["N"]} V={r["V"]} S={r["S"]} repeat {r["repeat"]}: {float(r["observation_cpu_s"]):.2f}')
    return '\n'.join(out) + '\n'


# --- synthetic checks ---------------------------------------------------------------------------------------------

NS, VS = (2, 4, 8, 16), (0, 1_000, 10_000, 100_000)


def table(metric, l_model, p_model, contended=lambda n, v: False, l_only=False, mark=None, ns=NS, held=None):
    rows = []
    for n_i, n in enumerate(ns):
        for v_i, v in enumerate(VS):
            for rep in range(3):
                jitter = 1 + 0.01 * ((rep * 7 + n_i * 3 + v_i) % 3 - 1)
                marks = (mark or (lambda n, v: {}))(n, v)
                actual = held(v, rep) if held else v
                rows.append({'test': 'L', 'variant': 'plain', 'N': n, 'V': v, 'V_actual': actual, 'S': 256,
                             'repeat': rep, 'contended': '1' if contended(n, v) else '0', 'lagged': 0, 'incomplete': 0,
                             metric: l_model(n, actual, n_i, v_i) * jitter, **marks})
                if not l_only:
                    rows.append({'test': 'P', 'variant': 'phone', 'N': n, 'V': v, 'V_actual': actual, 'S': 256,
                                 'repeat': rep, 'contended': '0', 'lagged': 0, 'incomplete': 0,
                                 metric: p_model(n, actual, n_i, v_i) * jitter})
    return [{k: (('1' if x else '0') if isinstance(x, bool) else str(x)) for k, x in r.items()} for r in rows]


def power(a, b, c, empty=1.0):
    """a·N^b·V^c, and `empty` times a·N^b for the empty pod."""
    return lambda n, v, *_: a * n ** b * (v ** c if v > 0 else empty)


def synthetic_cases():
    """(name, metric, rows, expected verdict, mutation[, check of the detail])."""
    m = 'r_cpu_s_per_record'
    yield ('a constant factor and equal slopes', m, table(m, power(1.0, 0.9, 0.05), power(3.0, 0.9, 0.05)),
           'honest', ('ratio_constant', lambda f: lambda r: False))
    yield ('the V slope differs', m, table(m, power(1.0, 0.9, 0.30), power(2.0, 0.9, 0.60)),
           'not honest', ('slopes_agree', lambda f: lambda a, b: 'same'))
    noisy = lambda n, v, i, j: 3.0 * n ** 0.5 * v ** 0.1 * (0.5 if (i + j) % 2 else 1.5)
    yield ('a phone table too noisy to tell the shapes apart', m, table(m, power(1.0, 0.5, 0.1), noisy),
           'too noisy', ('slopes_agree', lambda f: lambda a, b: 'same'))
    many = tuple(2 ** k for k in range(1, 11))
    wander = lambda n, v, i, j: 1.0 * n ** 0.5 * v ** 0.1 * (3.8 if j == 2 else 2.2)
    yield ('equal slopes and a ratio that wanders', m, table(m, power(1.0, 0.5, 0.1), wander, ns=many),
           'shape only', ('ratio_constant', lambda f: lambda r: True))
    yield ('the empty pod is a baseline, not a point of the V slope', m,
           table(m, power(1.0, 0.5, 0.0, empty=0.2), power(2.0, 0.5, 0.0, empty=1.0)),
           'honest', ('fit', lambda f: lambda c: f({k: (x, h if k[1] > 0 else 1.0) for k, (x, h) in c.items()}
                                                   | {(k[0], 1, k[2]): (x, 1.0) for k, (x, _) in c.items() if k[1] == 0})))
    drift = lambda v, rep: v + 600 * rep
    yield ('V is the count the pod held, not the level', m,
           table(m, power(1.0, 0.5, 0.4), power(2.0, 0.5, 0.4), held=drift),
           'honest', ('cells', lambda f: lambda rows, *a: f([{**r, 'V_actual': ''} for r in rows], *a)),
           lambda d: abs(d['fit_l'][2] - 0.4) < 0.02)
    t = 'steady_p50_s'
    inflated = lambda n, v, *_: 0.2 * n ** 0.3 * (max(v, 1)) ** 0.02 * (8.0 if n == 16 else 1.0)
    yield ('contended cells are left out of a time metric', t,
           table(t, inflated, power(0.6, 0.3, 0.02), contended=lambda n, v: n == 16),
           'honest', ('usable', lambda f: lambda r, kind: f(r, 'work')))
    yield ('lagged cells are left out of a time metric', t,
           table(t, inflated, power(0.6, 0.3, 0.02), mark=lambda n, v: {'lagged': 3} if n == 16 else {}),
           'honest', ('usable', lambda f: lambda r, kind: f(r, 'work')))
    e = 'sessions_per_record'
    undercounted = lambda n, v, *_: 1.0 * n ** 0.9 * (0.1 if n == 16 else 1.0)
    yield ('lagged cells are left out of a metric counted from events', e,
           table(e, undercounted, power(1.0, 0.9, 0.0), mark=lambda n, v: {'lagged': 3} if n == 16 else {}),
           'honest', ('usable', lambda f: lambda r, kind: f(r, 'work')))
    broken_catchup = lambda n, v, *_: 0.01 * n ** 0.05 * max(v, 1) ** 0.3 * (20.0 if v == 100_000 else 1.0)
    yield ('incomplete cells are left out of every fit', m,
           table(m, broken_catchup, power(0.02, 0.05, 0.3), mark=lambda n, v: {'incomplete': True} if v == 100_000 else {}),
           'honest', ('usable', lambda f: lambda r, kind: True))
    capped = lambda n, v: {'incomplete': True} if v > 1_000 else {}
    yield ('catch-up measured at one level with records fits its N slope alone', t,
           table(t, power(0.6, 0.3, 0.2), power(1.8, 0.3, 0.2), mark=capped),
           'honest', ('fit_n', lambda f: lambda pts: None),
           lambda d: d['fit_l'][2] is None and abs(d['fit_l'][0] - 0.3) < 0.02)
    hang = lambda n, v: {'join_hung': True} if v >= 10_000 else {}
    slow_then_hung = lambda n, v, *_: 0.5 * n ** 0.2 * max(v, 1) ** 0.1 * (1e4 if v >= 10_000 else 1.0)
    yield ('hung joins are left out of every fit', 'catchup_s',
           table('catchup_s', slow_then_hung, power(1.5, 0.2, 0.1), mark=hang),
           'honest', ('usable', lambda f: lambda r, kind: True))
    yield ('the laptop alone gives slopes and no verdict', m, table(m, power(1.0, 0.9, 0.05), None, l_only=True),
           'no phone', ('fit', lambda f: lambda c: None))


def synthetic():
    failures = 0
    for name, metric, rows, want, (attr, breaker), *checks in synthetic_cases():
        check = checks[0] if checks else (lambda d: True)
        got, detail = verdict(metric, rows, 'plain', 256)
        ok = got == want and check(detail)
        status = 'ok' if ok else f'FAILED: {got} != {want} or its check failed'
        failures += not ok
        original = globals()[attr]
        globals()[attr] = breaker(original)
        try:
            broken, broken_detail = verdict(metric, rows, 'plain', 256)
        finally:
            globals()[attr] = original
        bit = broken != want or not check(broken_detail)
        bite = f'bites ({broken})' if bit else 'DOES NOT BITE'
        failures += not bit
        print(f'{name}: {status}; broken {attr}: {bite}')
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / 'load.csv'
        rows = list(synthetic_cases())[0][2]
        with path.open('w', newline='') as f:
            w = csv.DictWriter(f, fieldnames=list(rows[0]))
            w.writeheader()
            w.writerows(rows)
        text = report(load(path))
        if 'honest, factor 3.00' not in text:
            failures += 1
            print(f'report does not state the factor:\n{text}')
        warned = [dict(r, thermal='1') if r['N'] == '16' and r['test'] == 'P' else r for r in rows]
        if 'N=16 V=0 S=256 repeat 0: thermal' not in report(warned):
            failures += 1
            print('report does not name a cell measured after its thermal wait ran out')
        hung = [dict(r, join_hung='1', evidence='hung/L-plain-N8-V10000-r0') if r['test'] == 'L' and r['N'] == '8'
                and r['V'] == '10000' and r['repeat'] == '0' else
                dict(r, not_run='join_hung') if r['test'] == 'L' and r['N'] == '8' and int(r['V']) > 10_000 else r
                for r in rows]
        text = report(hung)
        if not text.startswith('cells the platform did not finish\n  L/plain N=8 V=10000 S=256 repeat 0: join_hung — evidence hung/'):
            failures += 1
            print(f'report does not open with the hung cell:\n{text[:300]}')
        if 'N=8 V=100000 S=256 repeat 2: not_run: join_hung' not in text:
            failures += 1
            print('report does not list the cells a hung join left unrun')
        zeroed = [dict(r, r_cpu_s_per_record='0') if r['N'] == '2' and r['V'] == '1000' else r for r in rows]
        if 'N=2 V=1000 S=256 repeat 0: r_cpu_s_per_record is 0' not in report(zeroed):
            failures += 1
            print('report does not name a cell left out for a zero value')
    print('synthetic: ' + ('PASS' if not failures else f'{failures} FAILED'))
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('tables', type=Path, nargs='*', help="load.csv, one or more: a test L run's and a test P run's")
    ap.add_argument('--synthetic', action='store_true')
    args = ap.parse_args()
    if args.synthetic:
        return synthetic()
    if not args.tables:
        ap.error('a load.csv or --synthetic')
    sys.stdout.write(report([row for table in args.tables for row in load(table)]))
    return 0


if __name__ == '__main__':
    sys.exit(main())
