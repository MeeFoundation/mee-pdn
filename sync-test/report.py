#!/usr/bin/env -S python3 -I
"""Reports a ramp run against the last earlier run of the same profile, and keeps the page of every run's metrics.

`report RUN --history DIR --out DIR` reads RUN/ramp.csv, RUN/meta.json and r's events, writes summary.json, report.md
and a copy of ramp.csv into --out, and prints report.md: how each N's ramp ended, the changes past their threshold, and
each metric per N beside the baseline — the latest run in DIR/runs/ started before this one with the same profile.
`index DIR` rebuilds DIR/index.html from every DIR/runs/*/summary.json. `--synthetic` checks the comparison on runs
whose outcome is known, and that each check fails with its rule broken.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import csv
import html
import importlib.util
import json
import math
import os
import re
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location('load', HERE / 'load.py')
load = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(load)

# A metric's change is flagged past these. One ramp per N gives no repeats to tell a change from the runner's noise: a
# time metric moves with whatever else the runner does, a work or events metric far less (load.md "Work and time are
# reported apart").
THRESHOLD_PCT = {'time': 30, 'work': 15, 'events': 15}
CHECKPOINTS = (1_000, 5_000, 10_000, 20_000)
# name: (kind, label). A checkpoint's delay is the ramp's step that ends at that many records.
METRICS = {f'{q}_ms@{c}': ('time', f'{q} at {c:,}, ms') for c in CHECKPOINTS for q in ('p50', 'p95')}
DELAY = list(METRICS)
METRICS.update({
    'r_cpu_ms_per_record': ('work', 'r CPU, ms/record'), 'sessions_per_record': ('events', 'sessions/record'),
    'content_lag_p50_ms': ('time', 'r content lag p50, ms'), 'content_lag_p90_ms': ('time', 'r content lag p90, ms'),
    'catchup_s': ('time', 'catch-up, s'), 'catchup_records_per_s': ('time', 'catch-up, records/s'),
})
HIGHER_IS_BETTER = {'catchup_records_per_s'}
BEHIND_MS = load.RAMP_BEHIND_MS
MEE_PDN_URL = 'https://github.com/MeeFoundation/mee-pdn'
# The categorical slots of the dataviz reference palette, light and dark; a chart past their count is faceted.
SERIES = [('#2a78d6', '#3987e5'), ('#eb6834', '#d95926'), ('#1baf7a', '#199e70'), ('#eda100', '#c98500'),
          ('#e87ba4', '#d55181'), ('#008300', '#008300'), ('#6250d6', '#9085e9'), ('#e34948', '#e66767')]


# --- one run ---------------------------------------------------------------------------------------------------------

def number(x):
    return None if x in (None, '') else float(x)


def percentile(xs, q):
    """Nearest rank in sorted `xs`, as load.py takes a step's median and p95."""
    return xs[min(len(xs) - 1, int(q * len(xs)))] if xs else None


def profile(meta):
    """What two runs must share for their numbers to compare."""
    budget = (meta.get('host') or {}).get('cpu_budget') or meta.get('cpu_cores') or 0
    return {'test': meta['test'], 'variant': meta['variant'], 'rate': meta.get('ramp'), 'V': max(meta['V']),
            'N': sorted(meta['N']), 'pod_interval_s': meta['pod_interval_s'], 'catchup': not meta.get('skip_catchup'),
            'cpu_budget': round(float(budget), 1)}


def profile_label(p):
    return (f'{p["test"]} {p["variant"]} · {p["rate"]} records/s up to {p["V"]:,} · N {", ".join(map(str, p["N"]))} · '
            f'pod interval {p["pod_interval_s"]} s · {p["cpu_budget"]:g} CPUs · '
            + ('catch-up' if p['catchup'] else 'no catch-up'))


def ci_url():
    env = os.environ
    if env.get('GITHUB_RUN_ID'):
        return f'{env["GITHUB_SERVER_URL"]}/{env["GITHUB_REPOSITORY"]}/actions/runs/{env["GITHUB_RUN_ID"]}'
    return ''


def stop_of(last, target):
    """How a ramp ended, from its last step: one of load.py's two stops, the size reached, or the run cut short."""
    if last['stalled'] == '1':
        return 'stalled'
    if (number(last['p50_ms']) or 0) > BEHIND_MS:
        return 'behind'
    return 'reached' if int(last['held']) >= target else 'cut short'


def content_lags(path):
    """r's own clock from a record's insert to its content readable, ms, sorted; a record counts from its first
    delivery, since the catch-up delivers every record again."""
    inserted, lags = {}, {}
    with path.open() as f:
        for line in f:
            try:
                e = json.loads(line)
            except ValueError:  # the last line of a node stopped mid-write
                continue
            if e.get('store') != 'records':
                continue
            if e['kind'] == 'insert_remote':
                inserted.setdefault(e['hash'], e['t'])
            elif e['kind'] == 'content_ready' and e['hash'] in inserted:
                lags.setdefault(e['hash'], (e['t'] - inserted[e['hash']]) / 1e3)
    return sorted(lags.values())


def summarize(run):
    meta = json.loads((run / 'meta.json').read_text())
    with (run / 'ramp.csv').open() as f:
        rows = list(csv.DictReader(f))
    p = profile(meta)
    catchups = {c['N']: c for c in meta.get('ramp_catchup', [])}
    lines = []
    for n in p['N']:
        steps = [r for r in rows if int(r['N']) == n]
        if not steps:
            lines.append({'N': n, 'stop': 'not run', 'reached': 0, 'catchup': 'not run', 'host_max_pct': None,
                          'evidence': '', 'metrics': {}})
            continue
        last = steps[-1]
        metrics = {}
        for r in steps:
            # A stalled step's delays are of the records that did arrive.
            if int(r['held']) in CHECKPOINTS and r['stalled'] != '1':
                for q in ('p50', 'p95'):
                    metrics[f'{q}_ms@{r["held"]}'] = number(r[f'{q}_ms'])
        for m in ('r_cpu_ms_per_record', 'sessions_per_record'):
            xs = [x for x in (number(r[m]) for r in steps) if x is not None]
            metrics[m] = statistics.median(xs) if xs else None
        lags = sorted(x for path in (run / 'events').glob(f'n{n}s*-r0.jsonl') for x in content_lags(path))
        metrics['content_lag_p50_ms'], metrics['content_lag_p90_ms'] = percentile(lags, 0.5), percentile(lags, 0.9)
        c = catchups.get(n)
        if c:
            metrics['catchup_s'], metrics['catchup_records_per_s'] = (number(c.get('catchup_s')),
                                                                      number(c.get('catchup_records_per_s')))
        busy = [x for x in (number(r['host_share']) for r in steps) if x is not None]
        stop = stop_of(last, p['V'])
        lines.append({'N': n, 'stop': stop, 'reached': int(last['held']),
                      'catchup': 'not run' if not c else 'hung' if c['hung'] else 'incomplete' if c['incomplete']
                      else 'done',
                      'host_max_pct': max(busy) * 100 if busy else None,
                      'evidence': f'stalled/ramp-N{n}-at{last["held"]}' if stop == 'stalled' else '',
                      'metrics': {k: v for k, v in metrics.items() if v is not None}})
    revisions = meta.get('revisions') or {}
    return {'run': meta['run'], 'started': meta['started'], 'finished': meta.get('finished'),
            'mee_pdn': revisions.get('image') or revisions.get('workspace', ''),
            'ci': ci_url(),
            'host': meta.get('host') or {}, 'profile': p, 'lines': lines}


# --- the comparison --------------------------------------------------------------------------------------------------

def same_profile(a, b):
    return a['profile'] == b['profile']


def earlier(a, b):
    return a['started'] < b['started']


def history_runs(history):
    root = history / 'runs' if history else None
    return [json.loads(p.read_text()) for p in sorted(root.glob('*/summary.json'))] if root and root.is_dir() else []


def baseline(summary, runs):
    found = [s for s in runs if s['run'] != summary['run'] and same_profile(s, summary) and earlier(s, summary)]
    return max(found, key=lambda s: s['started'], default=None)


def delta(cur, prev):
    """The change in percent; `None` where either side has no value."""
    if cur is None or prev is None or prev == 0:
        return None
    return (cur - prev) / prev * 100


def state(line):
    return line['stop'], line['catchup']


def status(line):
    return f'{line["stop"]} at {line["reached"]:,} records, catch-up {line["catchup"]}'


def changes(cur, prev):
    """[{'change': 'worse' | 'better' | 'status', 'N', 'metric', 'text'}], N by N."""
    before = {b['N']: b for b in prev['lines']}
    out = []
    for line in cur['lines']:
        n, b = line['N'], before.get(line['N'])
        if b is None:
            continue
        if state(line) != state(b):
            out.append({'change': 'status', 'N': n, 'metric': None, 'text': f'N={n}: {status(b)} → {status(line)}'})
        for metric, (kind, label) in METRICS.items():
            d = delta(line['metrics'].get(metric), b['metrics'].get(metric))
            if d is None or abs(d) <= THRESHOLD_PCT[kind]:
                continue
            worse = (d > 0) != (metric in HIGHER_IS_BETTER)
            out.append({'change': 'worse' if worse else 'better', 'N': n, 'metric': metric,
                        'text': f'{label}, N={n}: {fmt(b["metrics"][metric])} → {fmt(line["metrics"][metric])} '
                                f'({d:+.0f} %; threshold ±{THRESHOLD_PCT[kind]} %)'})
    return out


# --- the report ------------------------------------------------------------------------------------------------------

def fmt(x):
    if x is None:
        return '–'
    if abs(x) >= 100:
        return f'{x:,.0f}'
    return f'{x:.1f}' if abs(x) >= 10 else f'{x:.3g}'


def when(ts):
    return time.strftime('%Y-%m-%d %H:%M UTC', time.gmtime(ts)) if ts else '–'


def day(ts):
    return time.strftime('%Y-%m-%d', time.gmtime(ts))


def rev_label(rev):
    """A revision as `meta.json` records it — `<sha>`, `<sha>+dirty`, `<sha> +dirty` — shortened."""
    return (rev or '?').split()[0].split('+')[0][:7] + ('+dirty' if 'dirty' in (rev or '') else '')


def commit_url(rev):
    """The mee-pdn commit a revision names, the one a dirty build started from included; '' for none."""
    sha = re.match(r'[0-9a-f]{7,40}\b', rev or '')
    return f'{MEE_PDN_URL}/commit/{sha.group(0)}' if sha else ''


def mee_pdn_md(summary):
    url = commit_url(summary['mee_pdn'])
    label = f'`{rev_label(summary["mee_pdn"])}`'
    return f'[{label}]({url})' if url else label


def mee_pdn_html(summary):
    url = commit_url(summary['mee_pdn'])
    label = f'<code>{html.escape(rev_label(summary["mee_pdn"]))}</code>'
    return f'<a href="{html.escape(url)}">{label}</a>' if url else label


def render(cur, prev):
    out = [f'# Load run `{cur["run"]}`', '',
           f'- mee-pdn {mee_pdn_md(cur)}'
           + (f'; [CI run]({cur["ci"]})' if cur['ci'] else ''),
           f'- Profile: {profile_label(cur["profile"])}',
           f'- Started {when(cur["started"])}'
           + (f', took {(cur["finished"] - cur["started"]) / 60:.0f} min' if cur['finished'] else ', did not finish')]
    if prev:
        out.append(f'- Compared with `{prev["run"]}` (mee-pdn {mee_pdn_md(prev)}, {when(prev["started"])})'
                   + (f', [its CI run]({prev["ci"]})' if prev['ci'] else ''))
    else:
        out.append('- No earlier run of this profile: nothing to compare with')
    out += ['', "## Each N's ramp", '', '| N | stop | records reached | catch-up | host busy, max % | evidence |',
            '|---|---|---|---|---|---|']
    out += [f'| {line["N"]} | {line["stop"]} | {line["reached"]:,} | {line["catchup"]} | {fmt(line["host_max_pct"])} | '
            f'{line["evidence"] or "–"} |' for line in cur['lines']]
    if prev:
        out += ['', '## Changes past the threshold', '',
                f'An N whose stop or catch-up changed; a metric past ±{THRESHOLD_PCT["time"]} % (time) or '
                f'±{THRESHOLD_PCT["work"]} % (work, events). One run per N: a flag is a lead to look at, not a verdict.',
                '']
        out += [f'- **{c["change"]}** {c["text"]}' for c in changes(cur, prev)] or ['None.']
    before = {b['N']: b for b in prev['lines']} if prev else {}

    def table(title, metrics):
        shown = [m for m in metrics if any(m in line['metrics'] for line in cur['lines'])]
        if not shown:
            return []
        rows = ['', f'### {title}', '', '| N | ' + ' | '.join(METRICS[m][1] for m in shown) + ' |',
                '|---|' + '---|' * len(shown)]
        for line in cur['lines']:
            b = before.get(line['N'])
            values = []
            for m in shown:
                d = delta(line['metrics'].get(m), b['metrics'].get(m)) if b else None
                values.append(fmt(line['metrics'].get(m)) + (f' ({d:+.0f} %)' if d is not None else ''))
            rows.append(f'| {line["N"]} | ' + ' | '.join(values) + ' |')
        return rows

    out += ['', '## Metrics', '',
            'Delay is from the write on a writer to the record readable on r, the median and the 95th percentile of '
            "the ramp's step that ends at that many records. r CPU and sessions per record are the median over the "
            "steps. Content lag is r's own clock from a record's insert to its content readable, over the whole ramp"
            + ('; in brackets the change against the baseline.' if prev else '.')]
    out += table('Delay', DELAY) + table('Work, content lag and catch-up', [m for m in METRICS if m not in DELAY])
    return '\n'.join(out) + '\n'


def report(run, history, out):
    summary = summarize(run)
    prev = baseline(summary, history_runs(history))
    text = render(summary, prev)
    out.mkdir(parents=True, exist_ok=True)
    (out / 'summary.json').write_text(json.dumps(summary, indent=1) + '\n')
    (out / 'report.md').write_text(text)
    if out.resolve() != run.resolve():
        shutil.copyfile(run / 'ramp.csv', out / 'ramp.csv')
    return text


# --- the history page ------------------------------------------------------------------------------------------------

PAGE = '''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pod load history</title>
<style>
:root {{ color-scheme: light; --surface: #fcfcfb; --text: #0b0b0b; --text-2: #52514e; --rule: #e4e3df;
  --grid: #ecebe7; {light} }}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{ color-scheme: dark; --surface: #1a1a19; --text: #f0efec; --text-2: #c3c2b7;
    --rule: #3a3936; --grid: #2c2b29; {dark} }}
}}
:root[data-theme="dark"] {{ color-scheme: dark; --surface: #1a1a19; --text: #f0efec; --text-2: #c3c2b7;
  --rule: #3a3936; --grid: #2c2b29; {dark} }}
body {{ margin: 0 auto; max-width: 1100px; padding: 24px 16px 64px; background: var(--surface); color: var(--text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }}
h1 {{ font-size: 24px; margin: 0 0 4px; }} h2 {{ font-size: 18px; margin: 40px 0 8px; }}
h3 {{ font-size: 15px; margin: 24px 0 4px; }}
.muted {{ color: var(--text-2); }}
.scroll {{ overflow-x: auto; }}
table {{ border-collapse: collapse; font-size: 13px; font-variant-numeric: tabular-nums; }}
th, td {{ text-align: left; padding: 4px 12px 4px 0; border-bottom: 1px solid var(--rule); white-space: nowrap; }}
a {{ color: var(--series-1); }}
.charts {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 8px 24px; }}
.legend {{ display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 13px; color: var(--text-2); margin: 4px 0; }}
.legend i {{ display: inline-block; width: 12px; height: 2px; margin: 0 6px 4px 0; vertical-align: middle; }}
svg {{ width: 100%; height: auto; display: block; }}
svg text {{ fill: var(--text-2); font-size: 11px; }}
details {{ font-size: 13px; color: var(--text-2); }}
</style>
</head>
<body>
<h1>Pod load history</h1>
<p class="muted">The ramp of <code>sync-test/load.md</code>, one section per profile. Each chart is one metric; a line is
one N, a point is a day's last run, placed by its date. A missing point is an N that run did not measure there: its ramp
stopped before that many records, or its catch-up did not finish. A point names its day, run and mee-pdn commit, and
opens the commit; "Values by day" lists the same. Updated {updated}.</p>
{sections}
</body>
</html>
'''


def series_vars(mode):
    return ' '.join(f'--series-{i + 1}: {pair[mode]};' for i, pair in enumerate(SERIES))


def ticks(highest):
    """Round gridline values from 0 up past `highest`, about 4 steps of 1, 2, 2.5 or 5 times a power of ten."""
    if highest <= 0:
        return [0, 1]
    raw = highest / 4
    mag = 10 ** math.floor(math.log10(raw))
    step = next(m * mag for m in (1, 2, 2.5, 5, 10) if m * mag >= raw)
    return [step * k for k in range(math.ceil(highest / step) + 1)]


def daily(runs):
    """The last run of each UTC day, oldest first: the day's point on the charts."""
    last = {}
    for r in sorted(runs, key=lambda s: s['started']):
        last[day(r['started'])] = r
    return list(last.values())


def chart(points, metric, ns):
    """One metric across `points` (oldest first, one run a day), a line per N, colored by the N's place in `ns` so an
    N keeps its color across the charts of a facet. A point names its day, run and mee-pdn commit, and opens the
    commit."""
    w, h, left, right, top, bottom = 360, 180, 44, 10, 10, 22
    label = METRICS[metric][1]
    values = [[next((line['metrics'].get(metric) for line in r['lines'] if line['N'] == n), None) for r in points]
              for n in ns]
    shown = [s_i for s_i, line in enumerate(values) if any(v is not None for v in line)]
    if not shown:
        return ''
    grid = ticks(max(v for line in values for v in line if v is not None))
    first, last = points[0]['started'], points[-1]['started']
    x = lambda i: left + (w - left - right) * ((points[i]['started'] - first) / (last - first) if last > first else 0.5)
    y = lambda v: top + (h - top - bottom) * (1 - v / grid[-1])
    parts = [f'<svg viewBox="0 0 {w} {h}" role="img" aria-label="{html.escape(label)}">']
    for v in grid:
        parts.append(f'<line x1="{left}" x2="{w - right}" y1="{y(v):.1f}" y2="{y(v):.1f}" stroke="var(--grid)"/>'
                     f'<text x="{left - 6}" y="{y(v) + 4:.1f}" text-anchor="end">{v:,.6g}</text>')
    # The first and the last day, anchored inward so neither runs off the plot.
    ends = [(0, 'middle')] if len(points) == 1 else [(0, 'start'), (len(points) - 1, 'end')]
    for i, anchor in ends:
        parts.append(f'<text x="{x(i):.1f}" y="{h - 6}" text-anchor="{anchor}">{day(points[i]["started"])}</text>')
    for s_i, (n, line) in enumerate(zip(ns, values)):
        color = f'var(--series-{s_i + 1})'
        xy = [(x(i), y(v)) for i, v in enumerate(line) if v is not None]
        if len(xy) > 1:
            path = ' '.join(f'{px:.1f},{py:.1f}' for px, py in xy)
            parts.append(f'<polyline points="{path}" fill="none" stroke="{color}" stroke-width="2" '
                         f'stroke-linejoin="round" stroke-linecap="round"/>')
        for i, v in enumerate(line):
            if v is None:
                continue
            r = points[i]
            tip = f'{day(r["started"])} · N={n}: {fmt(v)} · run {r["run"]} · mee-pdn {rev_label(r["mee_pdn"])}'
            # A transparent ring around the mark: a target larger than the 8 px point.
            dot = (f'<circle cx="{x(i):.1f}" cy="{y(v):.1f}" r="9" fill="transparent"/>'
                   f'<circle cx="{x(i):.1f}" cy="{y(v):.1f}" r="4" fill="{color}" stroke="var(--surface)" '
                   f'stroke-width="2"/><title>{html.escape(tip)}</title>')
            url = commit_url(r['mee_pdn'])
            parts.append(f'<a href="{html.escape(url)}">{dot}</a>' if url else f'<g>{dot}</g>')
    parts.append('</svg>')
    legend = ''.join(f'<span><i style="background: var(--series-{s_i + 1})"></i>N={ns[s_i]}</span>' for s_i in shown)
    rows = ''.join(f'<tr><td>{day(r["started"])}</td><td>{mee_pdn_html(r)}</td>'
                   f'<td><a href="{html.escape(run_link(r))}">{html.escape(r["run"])}</a></td>'
                   + ''.join(f'<td>{fmt(values[s_i][i])}</td>' for s_i in shown) + '</tr>'
                   for i, r in reversed(list(enumerate(points))))
    table = (f'<details><summary>Values by day</summary><div class="scroll"><table><tr><th>day</th><th>mee-pdn</th>'
             '<th>run</th>' + ''.join(f'<th>N={ns[s_i]}</th>' for s_i in shown) + f'</tr>{rows}</table></div></details>')
    return f'<div><h3>{html.escape(label)}</h3><div class="legend">{legend}</div>{"".join(parts)}{table}</div>'


def run_link(summary):
    """The run's report.md: on GitHub, rendered, when the CI names the repository; else beside the page."""
    env = os.environ
    path = f'runs/{summary["run"]}/report.md'
    if env.get('GITHUB_REPOSITORY'):
        branch = env.get('HISTORY_BRANCH', 'load-history')
        return f'{env.get("GITHUB_SERVER_URL", "https://github.com")}/{env["GITHUB_REPOSITORY"]}/blob/{branch}/{path}'
    return path


def reached_of(summary):
    return f'{sum(line["stop"] == "reached" for line in summary["lines"])}/{len(summary["lines"])}'


def ci_link(summary):
    return f'<a href="{html.escape(summary["ci"])}">CI</a>' if summary['ci'] else '–'


def index(history):
    runs = sorted(history_runs(history), key=lambda s: s['started'])
    groups = {}
    for r in runs:
        groups.setdefault(json.dumps(r['profile'], sort_keys=True), []).append(r)
    sections = []
    for group in sorted(groups.values(), key=lambda g: -g[-1]['started']):
        points = daily(group)
        charted = {r['run'] for r in points}
        table = ''.join(
            f'<tr{"" if r["run"] in charted else " class=muted"}><td><a href="{html.escape(run_link(r))}">'
            f'{html.escape(r["run"])}</a></td><td>{when(r["started"])}</td><td>{mee_pdn_html(r)}</td>'
            f'<td>{reached_of(r)}</td>'
            f'<td>{"the day&#39;s point" if r["run"] in charted else "a later run that day"}</td><td>{ci_link(r)}</td></tr>'
            for r in reversed(group))
        out = [f'<h2>{html.escape(profile_label(group[-1]["profile"]))}</h2>',
               '<div class="scroll"><table><tr><th>run</th><th>started</th><th>mee-pdn</th>'
               f'<th>N reached</th><th>charts</th><th></th></tr>{table}</table></div>']
        ns = sorted({line['N'] for r in points for line in r['lines']})
        metrics = [m for m in METRICS if any(m in line['metrics'] for r in points for line in r['lines'])]
        for i in range(0, len(ns), len(SERIES)):
            facet = ns[i:i + len(SERIES)]
            if len(ns) > len(SERIES):
                out.append(f'<h3>N {", ".join(map(str, facet))}</h3>')
            out.append(f'<div class="charts">{"".join(chart(points, m, facet) for m in metrics)}</div>')
        sections.append('\n'.join(out))
    page = PAGE.format(light=series_vars(0), dark=series_vars(1), updated=when(time.time()),
                       sections='\n'.join(sections) or '<p>No runs yet.</p>')
    (history / 'index.html').write_text(page)
    (history / '.nojekyll').touch()


# --- the self-check --------------------------------------------------------------------------------------------------

def synthetic_run(root, name, started, ns=(2, 4), rate=20, values=None, stalled=()):
    """A ramp run of test L up to 1,000 records: every step's value 1.0 unless `values` names it; the N in `stalled`
    stop at 500, the others catch up. r's events give lags of 1 to 10 ms, and the catch-up delivers each record again
    1 s late."""
    run = root / name
    (run / 'events').mkdir(parents=True)
    value = lambda m: (values or {}).get(m, 1.0)
    meta = {'run': name, 'test': 'L', 'variant': 'plain', 'N': list(ns), 'V': [1000], 'S': [256], 'ramp': rate,
            'pod_interval_s': 300, 'skip_catchup': False, 'host': {'cpu_budget': 4.0}, 'revisions': {'image': name},
            'started': started, 'finished': started + 60,
            'ramp_catchup': [{'N': n, 'catchup_s': value('catchup_s'), 'catchup_records_per_s':
                              value('catchup_records_per_s'), 'incomplete': False, 'hung': False, 'ready': 1000}
                             for n in ns if n not in stalled]}
    (run / 'meta.json').write_text(json.dumps(meta))
    with (run / 'ramp.csv').open('w', newline='') as f:
        w = csv.DictWriter(f, load.RAMP_COLUMNS)
        w.writeheader()
        for n in ns:
            for held in (100, 200, 300, 400, 500) if n in stalled else (100, 200, 300, 400, 500, 1000):
                w.writerow({'run': name, 'N': n, 'held': held, 'records': 100, 'p50_ms': value('p50_ms'),
                            'p95_ms': value('p95_ms'), 'r_cpu_ms_per_record': value('r_cpu_ms_per_record'),
                            'r_cpu_pct': 1.0, 'host_share': 0.5, 'write_call_p50_ms': 1.0,
                            'sessions_per_record': value('sessions_per_record'),
                            'stalled': int(n in stalled and held == 500)})
        events = [{'store': 'membership', 'kind': 'content_ready', 'hash': 'm', 't': 0}]
        for again in (0, 1_000_000):
            for k in range(10):
                t = 10_000_000 * (k + 1) + again
                events += [{'store': 'records', 'kind': 'insert_remote', 'hash': f'h{k}', 't': t},
                           {'store': 'records', 'kind': 'content_ready', 'hash': f'h{k}', 't': t + 1_000 * (k + 1)}]
        events.append({'store': 'records', 'kind': 'insert_remote', 'hash': 'never ready', 't': 1})
        for n in ns:
            (run / 'events' / f'n{n}s256-r0.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in events))
    return run


def synthetic():
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        history = root / 'history'
        runs = root / 'runs'
        # a: the baseline; c: started between a and b at another rate; d: the same profile, after b.
        for name, started, kw in (('a', 100, {}), ('c', 150, {'rate': 40, 'values': {'r_cpu_ms_per_record': 9.0}}),
                                  ('d', 300, {'values': {'r_cpu_ms_per_record': 9.0}})):
            report(synthetic_run(runs, name, started, **kw), history, history / 'runs' / name)
        b_run = synthetic_run(runs, 'b', 200, stalled={4}, values={
            'p50_ms': 1.2, 'r_cpu_ms_per_record': 1.2, 'catchup_records_per_s': 0.6, 'sessions_per_record': 0.8})
        b = summarize(b_run)
        alone = summarize(synthetic_run(runs, 'e', 400, ns=(2, 4, 8)))

        def flagged():
            cur = summarize(b_run)
            prev = baseline(cur, history_runs(history))
            return {(c['change'], c['metric'], c['N']) for c in changes(cur, prev)} if prev else set()

        want = {('worse', 'r_cpu_ms_per_record', 2), ('worse', 'catchup_records_per_s', 2),
                ('better', 'sessions_per_record', 2), ('status', None, 4), ('worse', 'r_cpu_ms_per_record', 4),
                ('better', 'sessions_per_record', 4)}
        step = lambda stalled='0', p50='1', held='1000': {'stalled': stalled, 'p50_ms': p50, 'held': held}
        cases = [
            ('a run is compared with the latest earlier run of its profile',
             lambda: (baseline(b, history_runs(history)) or {}).get('run'), 'a',
             [('same_profile', lambda f: lambda x, y: True), ('earlier', lambda f: lambda x, y: True)]),
            ('a run with no earlier run of its profile has no baseline',
             lambda: baseline(alone, history_runs(history)), None, [('same_profile', lambda f: lambda x, y: True)]),
            ('each change past its threshold is flagged, and only those', flagged, want,
             [('THRESHOLD_PCT', lambda f: {'time': 15, 'work': 15, 'events': 15}),
              ('HIGHER_IS_BETTER', lambda f: set()), ('state', lambda f: lambda line: ())]),
            ("a ramp's stop is read from its last step",
             lambda: [stop_of(s, 1000) for s in (step(stalled='1'), step(p50='40000'), step(held='500'), step())],
             ['stalled', 'behind', 'cut short', 'reached'], [('BEHIND_MS', lambda f: 10 ** 9)]),
            ("r's content lag counts each record from its first delivery",
             lambda: [summarize(runs / 'a')['lines'][0]['metrics'][m] for m in ('content_lag_p50_ms',
                                                                                 'content_lag_p90_ms')],
             [6.0, 10.0], [('percentile', lambda f: lambda xs, q: xs[-1] if xs else None)]),
            ('a day is charted by its last run',
             lambda: [r['run'] for r in daily([{'run': 'late', 'started': 7_200}, {'run': 'early', 'started': 3_600},
                                               {'run': 'next day', 'started': 90_000}])],
             ['late', 'next day'], [('day', lambda f: lambda ts: str(ts))]),
        ]
        for name, got_of, want_, breakers in cases:
            got = got_of()
            ok = got == want_
            failures += not ok
            bites = []
            for attr, breaker in breakers:
                original = globals()[attr]
                globals()[attr] = breaker(original)
                try:
                    bit = got_of() != want_
                finally:
                    globals()[attr] = original
                failures += not bit
                bites.append(f'broken {attr}: {"bites" if bit else "DOES NOT BITE"}')
            print(f'{name}: {"ok" if ok else f"FAILED: {got!r} != {want_!r}"}; {"; ".join(bites)}')
        text = render(b, baseline(b, history_runs(history)))
        for needle in ('Compared with `a`', '| 4 | stalled | 500 | not run |', 'stalled/ramp-N4-at500', '1.2 (+20 %)',
                       '**status** N=4: reached at 1,000 records, catch-up done → stalled at 500 records, '
                       'catch-up not run'):
            if needle not in text:
                failures += 1
                print(f'report lacks {needle!r}:\n{text}')
        links = {'62b008390a7ab94553ebd87640f9db29289ea18d +dirty': ('62b0083+dirty', '/commit/62b008390a7ab94553ebd87640f9db29289ea18d'),
                 '14ad3c5+dirty': ('14ad3c5+dirty', '/commit/14ad3c5'), 'example': ('example', '')}
        for rev, (label, url) in links.items():
            if rev_label(rev) != label or not commit_url(rev).endswith(url) or bool(url) != bool(commit_url(rev)):
                failures += 1
                print(f'{rev!r} reads as {rev_label(rev)!r}, {commit_url(rev)!r}')
        if 'No earlier run of this profile' not in render(alone, None):
            failures += 1
            print('a report without a baseline does not say so')
        index(history)
        page = (history / 'index.html').read_text()
        if not all(f'>{name}</a>' in page for name in ('a', 'c', 'd')) or page.count('<h2>') != 2:
            failures += 1
            print('the history page does not list every run under its profile')
    print('synthetic: ' + ('PASS' if not failures else f'{failures} FAILED'))
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest='command')
    rep = sub.add_parser('report', help='summarize a ramp run, compare it, print report.md')
    rep.add_argument('run', type=Path)
    rep.add_argument('--history', type=Path, help='the history checkout whose runs/ holds earlier runs')
    rep.add_argument('--out', type=Path, help='where summary.json, report.md and ramp.csv go; default the run')
    idx = sub.add_parser('index', help='rebuild DIR/index.html')
    idx.add_argument('history', type=Path)
    ap.add_argument('--synthetic', action='store_true')
    args = ap.parse_args()
    if args.synthetic:
        return synthetic()
    if args.command == 'report':
        if not (args.run / 'ramp.csv').exists():
            sys.exit(f'{args.run} holds no ramp.csv: report reads a --ramp run')
        sys.stdout.write(report(args.run, args.history, args.out or args.run))
        return 0
    if args.command == 'index':
        index(args.history)
        return 0
    ap.error('report, index or --synthetic')


if __name__ == '__main__':
    sys.exit(main())
