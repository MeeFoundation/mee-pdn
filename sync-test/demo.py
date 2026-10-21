#!/usr/bin/env -S python3 -I
"""A live dashboard of load.md's test L for an audience: one pod of N pdn-node-http nodes on this laptop, a prefill,
live writes, and a wiped reader's catch-up, drawn full-screen in the terminal from the nodes' event streams.

Nodes, pod, fill, steady and catch-up are load.py's own functions; the run directory is tmp/load/demo-<timestamp>/,
laid out like load.py's. `--rehearse` draws the same dashboard from synthetic events at real speed, with no node;
`--rehearse=hang` rehearses a join that does not answer. DEMO_FRAMES=<file> appends one plain-text frame a second.
DEMO.md is the presenter's run-sheet.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import collections
import importlib.util
import json
import math
import os
import random
import re
import select
import shutil
import signal
import statistics
import termios
import textwrap
import threading
import time
import tty
from pathlib import Path

_spec = importlib.util.spec_from_file_location('load', Path(__file__).with_name('load.py'))
load = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(load)

PHASES = ['Start nodes', 'Form pod', 'Prefill', 'Live writes', 'Catch-up', 'Result']
S = 256
# Stage-sized bounds in place of load.py's unattended ones: a stalled prefill, a join that does not answer and a
# silent node are each called within a minute rather than after a pod interval.
FILL_BOUND_S, JOIN_BOUND_S, SILENT_S, DRAIN_S = 60, 30, 15, 15
HOLDS = {'start': 1, 'form': 3, 'prefill': 4, 'live': 3, 'catchup': 6}
SPARK = ' ▁▂▃▄▅▆▇█'
VIEW = None


def label(name):
    """'n4s256-w2' -> 'w2'; every incarnation of the reader is 'r'."""
    short = name.rsplit('-', 1)[-1]
    return 'r' if short.startswith('r') else short


def ms(x):
    return '  –  ' if x is None else f'{x * 1000:,.0f} ms'


class View:
    """Everything the dashboard draws, fed by the event streams and the driver; every access under `lock`."""

    def __init__(self, args):
        self.lock = threading.RLock()
        self.args = args
        self.phase, self.phase_t0, self.note = 0, time.monotonic(), ''
        self.nodes = {}                          # stream name -> {'held': set, 'flash': t, 'events': n}
        self.names = {}                          # identity / node id -> label
        self.pending = {}                        # (stream, hash) -> insert_remote awaiting its content
        self.seen = set()                        # membership hashes and session ids already in the ticker
        self.ticker = collections.deque(maxlen=6)
        self.tick_at = collections.defaultdict(float)
        self.window = None                       # prefill or live: placed, readable, latencies, sessions
        self.catchup = None
        self.result = None
        self.error = None
        self.sampler = None
        self.roster = lambda: []                 # [(stream name, pid)] of the live nodes
        self.run_dir = None

    def set_phase(self, i):
        a = self.args
        note = [f'Starting {a.n} pdn-node-http processes on this laptop; each is a whole node with its own store.',
                'w1 creates a pod; every other node joins it with an invite from w1.',
                f'The writers place {a.v:,} records of {S} B as fast as the nodes take them; r writes nothing and '
                'receives every one.',
                f'The writers place {a.rate} records a second for {a.steady_s:g} s; each record is timed from its '
                'write to readable on r.',
                'r leaves the pod and its directory is wiped; it joins again with nothing and catches up.',
                'Result'][i]
        with self.lock:
            self.phase, self.phase_t0, self.note = i, time.monotonic(), note

    def tick(self, text, kind=None, every=0.0, style=''):
        now = time.monotonic()
        if kind and now - self.tick_at[kind] < every:
            return
        self.tick_at[kind] = now
        self.ticker.append((time.strftime('%H:%M:%S'), text, style))

    def learn(self, name, *ids):
        with self.lock:
            for i in ids:
                self.names[i] = label(name)

    def open_window(self):
        with self.lock:
            self.window = {'t0_us': load.now_us(), 'placed': 0, 'readable': 0, 'lat': [], 'sessions': set()}

    def opened(self, name):
        with self.lock:
            if self.catchup and self.catchup['name'] == name:
                self.catchup['t0'] = time.time()

    def on_event(self, name, rec):
        with self.lock:
            node = self.nodes.setdefault(name, {'held': set(), 'flash': 0.0, 'events': 0})
            node['events'] += 1
            kind, store, w = rec['kind'], rec.get('store'), self.window
            if store == 'membership' and kind in ('insert_local', 'insert_remote'):
                segs = rec['key'].split('/')
                if len(segs) == 6 and rec['hash'] not in self.seen:
                    self.seen.add(rec['hash'])
                    # The rejoined r starts inside load.catchup, so its identity is the one never learned.
                    who = self.names.get(segs[1], 'r' if self.phase == 4 else 'a member')
                    verb = {'created': 'created the pod', 'joined': 'joined the pod', 'left': 'left the pod',
                            'removed': 'was removed from the pod'}.get(segs[3])
                    if verb:
                        self.tick(f'{who} {verb}', style='1')
                if kind == 'insert_remote':
                    node['flash'] = time.monotonic()
            if store != 'records':
                return
            if kind == 'insert_local':
                node['held'].add(rec['hash'])
                if w and rec['t'] >= w['t0_us']:
                    w['placed'] += 1
            elif kind == 'insert_remote':
                if rec['content_status'] == 'complete':
                    self.arrive(name, node, rec, rec['t'])
                else:
                    self.pending[(name, rec['hash'])] = rec
            elif kind == 'content_ready':
                pend = self.pending.pop((name, rec['hash']), None)
                if pend:
                    self.arrive(name, node, pend, rec['t'])
            elif kind == 'sync_finished' and rec.get('ok') and rec.get('session') and (rec.get('received') or 0) > 0:
                if w and rec['finished'] >= w['t0_us']:
                    w['sessions'].add(rec.get('session'))
                if rec.get('session') not in self.seen:
                    self.seen.add(rec.get('session'))
                    peer = self.names.get(rec['peer'], 'r' if self.phase == 4 else 'a peer')
                    got = rec['received']
                    self.tick(f'session {label(name)} ⇄ {peer}   ok, {got} {"entry" if got == 1 else "entries"}',
                              kind='session', every=1.5)

    def arrive(self, name, node, rec, t):
        node['held'].add(rec['hash'])
        node['flash'] = time.monotonic()
        if label(name) != 'r':
            return
        if self.catchup and self.catchup['name'] == name:
            self.catchup['last'] = t / 1e6
        w = self.window
        author = self.names.get(rec['key'].split('/')[1], 'a writer')
        count = len(node['held'])
        if self.phase == 3 and w and t >= w['t0_us']:
            lat = (t - rec['entry_ts']) / 1e6
            w['readable'] += 1
            w['lat'].append((t / 1e6, lat))
            self.tick(f'{author} → r   record {count:,} readable   {lat * 1000:.0f} ms', kind='arrive', every=0.7)
        elif self.phase == 2 and w and t >= w['t0_us']:
            w['readable'] += 1
            self.tick(f'{author} → r   record {count:,} arrived', kind='arrive', every=0.7)
        elif self.phase == 4:
            self.tick(f'{author} → r   record {count:,} back on the rejoined r', kind='arrive', every=0.7)

    def held(self, name):
        node = self.nodes.get(name)
        return len(node['held']) if node else 0


class DemoStream(load.Stream):
    """load.py's Stream that also feeds VIEW; load.Node.open_stream builds it under the name `load.Stream`."""

    def __init__(self, path, track, name=None):
        super().__init__(path, track)
        self.name = name or path.stem
        VIEW.opened(self.name)

    def feed(self, line):
        super().feed(line)
        VIEW.on_event(self.name, json.loads(line))


# --- the live run ----------------------------------------------------------------------------------------------------

class Live:
    def __init__(self, args, view):
        if not (load.ROOT / 'Cargo.toml').exists():
            sys.exit(f'{load.ROOT} is not the PDN workspace')
        load.STEADY_S, load.STEADY_RATE = args.steady_s, args.rate
        load.FILL_MIN_S, load.CATCHUP_MIN_S, load.SILENT_S, load.DRAIN_S = FILL_BOUND_S, JOIN_BOUND_S, SILENT_S, DRAIN_S
        load.Stream = DemoStream
        ns = argparse.Namespace(run_id=time.strftime('demo-%Y%m%d-%H%M%S'), bin=args.bin, n=[args.n], v=[args.v],
                                s=[S], repeats=1, variant='plain', obs_cost=False, pod_interval=None)
        # Run's preflight exits on a pdn-node-http it did not start, so a measurement run is never disturbed.
        try:
            self.run = load.Run(ns)
        except SystemExit:
            shutil.rmtree(load.ROOT / 'tmp' / 'load' / ns.run_id, ignore_errors=True)
            raise
        self.run.meta['demo'] = {'steady_s': args.steady_s, 'rate': args.rate, 'join_bound_s': JOIN_BOUND_S,
                                 'silent_s': SILENT_S, 'drain_s': DRAIN_S, 'fill_bound_s': FILL_BOUND_S}
        self.view, self.args, self.pod, self.stopping = view, args, None, threading.Lock()
        view.sampler, view.run_dir = self.run.sampler, self.run.dir.relative_to(load.ROOT)
        view.roster = lambda: [(nd.name, nd.pid) for nd in list(self.run.live.values())]

    def drive(self):
        view, args, run = self.view, self.args, self.run
        try:
            self.steps(view, args, run)
        finally:
            self.stop()

    def steps(self, view, args, run):
        n, v = args.n, args.v
        view.set_phase(0)
        pod = self.pod = load.Pod(run, n, S)
        nodes = []
        for name in [f'{pod.prefix}-w{i}' for i in range(1, n)] + [f'{pod.prefix}-r0']:
            nd = load.Node(run, name).start()
            node_id = next((ln.split()[1] for ln in nd.status() if ln.startswith('node ')), '')
            view.learn(name, nd.identity, node_id)
            with view.lock:
                view.tick(f'{label(name)} started   pid {nd.pid}')
            nodes.append(nd)
        time.sleep(HOLDS['start'])

        view.set_phase(1)
        owner = nodes[0]
        pod.pod = owner.call('POST', '/pods')['pod']
        pod.writers.append(owner)
        pod.streams.append(owner.open_stream(pod.pod, track=False))
        for nd in nodes[1:-1]:
            pod.writers.append(pod.join(nd))
        pod.r = pod.join(nodes[-1], track=True)
        time.sleep(HOLDS['form'])

        view.set_phase(2)
        view.open_window()
        why, evidence = pod.fill(v)
        if why:
            raise RuntimeError(f'prefill: {why}' + (f'; evidence in {run.dir / evidence}' if evidence else ''))
        time.sleep(HOLDS['prefill'])

        view.set_phase(3)
        view.open_window()
        cell = {'N': n, 'V': v, 'S': S, 'repeat': 0, 'V_actual': pod.held, 'started': time.time()}
        run.meta['cells'].append(cell)
        values, steady_short, _, _ = load.steady(run, pod, cell)
        time.sleep(HOLDS['live'])

        held = pod.held
        with view.lock:
            view.catchup = {'name': f'{pod.prefix}-r{pod.incarnation + 1}', 'target': held, 't0': None,
                            'state': 'running', 'evidence': None}
        view.set_phase(4)
        cu_values, cu_short, hung, (joiner, inviter, log_from), _, _ = load.catchup(run, pod, cell)
        values.update(cu_values)
        evidence = ''
        if hung:
            evidence = load.keep_evidence(run, f'hung/demo-N{n}-V{v}-S{S}', [joiner, inviter], log_from)
            joiner.stop()
        state = 'hung' if hung else 'incomplete' if cu_short else 'done'
        with view.lock:
            view.catchup.update(state=state, evidence=view.run_dir / evidence if evidence else None)
        run.row(n, v, S, 0, cell['V_actual'], join_hung='1' if hung else '0', evidence=evidence,
                incomplete='1' if (steady_short or cu_short) and not hung else '0', **values)
        run.save()
        time.sleep(HOLDS['catchup'])

        num = lambda k: float(values[k]) if values.get(k) else None
        with view.lock:
            view.result = {'p50': num('steady_p50_s'), 'p90': num('steady_p90_s'), 'p99': num('steady_p99_s'),
                           'cpu': num('r_cpu_s_per_record'), 'spr': num('sessions_per_record'),
                           'catchup_s': num('catchup_s'), 'catchup_rate': num('catchup_records_per_s'),
                           'held': held, 'state': state, 'evidence': view.catchup['evidence']}
        view.set_phase(5)

    def stop(self):
        """Stops every node this demo started and keeps the run directory but the nodes' stores; idempotent."""
        with self.stopping:
            pod, self.pod = self.pod, None
            if pod:
                pod.stop()
            self.run.stop_all()
            shutil.rmtree(self.run.dir / 'nodes', ignore_errors=True)
            self.run.meta['finished'] = time.time()
            self.run.save()


# --- the rehearsal ---------------------------------------------------------------------------------------------------

class FakeSampler(threading.Thread):
    """Sampler's `samples` shape from synthetic CPU: each node's share follows the events it handled that second."""

    def __init__(self, view):
        super().__init__(daemon=True)
        self.view = view
        self.samples = collections.deque(maxlen=4_000)
        self.cpu = collections.defaultdict(float)
        self.seen = collections.Counter()

    def run(self):
        rng = random.Random(7)
        while True:
            t = time.monotonic()
            procs = {}
            with self.view.lock:
                for name, pid in self.view.roster():
                    events = self.view.nodes.get(name, {}).get('events', 0)
                    eps, self.seen[name] = events - self.seen[name], events
                    self.cpu[pid] += min(0.9, (0.4 + 0.8 * eps) / 100) * rng.uniform(0.85, 1.05)
                    procs[pid] = (self.cpu[pid], int((29 + 4 * math.log1p(self.view.held(name) / 500)) * 1024))
            self.samples.append((t, time.time(), procs))
            time.sleep(max(0.0, 1.0 - (time.monotonic() - t)))


class FakeNode:
    def __init__(self, name, pid):
        self.name, self.pid = name, pid
        self.identity, self.node_id, self.sub = os.urandom(32).hex(), os.urandom(32).hex(), os.urandom(8).hex()
        self.seq = 0
        self.stream = DemoStream(None, track=label(name) == 'r', name=name)

    def emit(self, t, **rec):
        rec = {'sub': self.sub, 'seq': self.seq, 't': int(t * 1e6), 'pod': 'rehearsal'} | rec
        self.seq += 1
        self.stream.feed(json.dumps(rec))


class Rehearsal:
    """Synthetic events at real speed, sized on the 9 October laptop run at N = 4: steady p50 about 250 ms and p99
    about 800 ms, one session per receiving node per record, catch-up about 1,000 records/s."""

    def __init__(self, args, view):
        self.view, self.args = view, args
        self.rng = random.Random(42)
        self.live, self.members, self.records = [], [], []
        view.roster = lambda: [(nd.name, nd.pid) for nd in self.live]
        view.sampler = FakeSampler(view)
        view.sampler.start()

    def play(self, events):
        """events: (wall time, node, record fields); each fed when its time comes."""
        for at, node, rec in sorted(events, key=lambda e: e[0]):
            time.sleep(max(0.0, at - time.time()))
            with self.view.lock:
                node.emit(at, **rec)

    def member(self, actor, subject, kind, at):
        key = f'member/{subject.identity}/1/{kind}/{actor.identity}/1'
        h = os.urandom(32).hex()
        events = [(at, actor, {'store': 'membership', 'kind': 'insert_local', 'key': key, 'entry_ts': int(at * 1e6),
                               'hash': h, 'len': 64})]
        for nd in self.members:
            if nd is not actor:
                events.append((at + 0.05, nd, {'store': 'membership', 'kind': 'insert_remote', 'key': key,
                                               'entry_ts': int(at * 1e6), 'hash': h, 'len': 64,
                                               'from': actor.node_id, 'content_status': 'complete'}))
        return events

    def writes(self, count, rate, median_s, sigma, start):
        writers = [nd for nd in self.members if label(nd.name) != 'r']
        events = []
        for k in range(count):
            w = writers[k % len(writers)]
            at = start + k / rate
            key = f'by/{w.identity}/immutable-document/{os.urandom(16).hex()}/1'
            h = os.urandom(32).hex()
            self.records.append((key, h, int(at * 1e6), w))
            events.append((at, w, {'store': 'records', 'kind': 'insert_local', 'key': key, 'entry_ts': int(at * 1e6),
                                   'hash': h, 'len': S}))
            for nd in self.members:
                if nd is w:
                    continue
                ready = at + median_s * math.exp(self.rng.gauss(0, sigma))
                complete = self.rng.random() < 0.5
                base = {'store': 'records', 'key': key, 'entry_ts': int(at * 1e6), 'hash': h, 'len': S,
                        'from': w.node_id}
                events.append((ready - (0 if complete else 0.003), nd, base | {
                    'kind': 'insert_remote', 'content_status': 'complete' if complete else 'missing'}))
                if not complete:
                    events.append((ready, nd, {'store': 'records', 'kind': 'content_ready', 'hash': h}))
                for _ in range(2 if self.rng.random() < 0.1 else 1):
                    sess = os.urandom(16).hex()
                    common = {'store': 'records', 'kind': 'sync_finished', 'session': sess, 'ok': True,
                              'started': int((ready - 0.02) * 1e6), 'finished': int(ready * 1e6)}
                    events.append((ready, nd, common | {'peer': w.node_id, 'origin': 'accept', 'sent': 0,
                                                        'received': 1}))
                    events.append((ready, w, common | {'peer': nd.node_id, 'origin': 'connect', 'sent': 1,
                                                       'received': 0}))
        return events

    def add(self, name):
        nd = FakeNode(name, self.rng.randrange(40_000, 99_999))
        self.view.learn(name, nd.identity, nd.node_id)
        with self.view.lock:
            self.live.append(nd)
        return nd

    def drive(self):
        view, args = self.view, self.args
        n, v, prefix = args.n, args.v, f'n{args.n}s{S}'
        view.set_phase(0)
        nodes = []
        for name in [f'{prefix}-w{i}' for i in range(1, n)] + [f'{prefix}-r0']:
            time.sleep(0.7)
            nodes.append(self.add(name))
            with view.lock:
                view.tick(f'{label(name)} started   pid {nodes[-1].pid}')
        time.sleep(HOLDS['start'])

        view.set_phase(1)
        self.members = nodes[:1]
        self.play(self.member(nodes[0], nodes[0], 'created', time.time() + 0.3))
        for nd in nodes[1:]:
            self.members.append(nd)
            self.play(self.member(nodes[0], nd, 'joined', time.time() + 0.9))
        time.sleep(HOLDS['form'])

        view.set_phase(2)
        view.open_window()
        self.play(self.writes(v, 150, 0.15, 0.5, time.time() + 0.5))
        time.sleep(HOLDS['prefill'])

        view.set_phase(3)
        view.open_window()
        r0 = nodes[-1]
        t0 = time.monotonic()
        self.play(self.writes(int(args.steady_s * args.rate), args.rate, 0.25, 0.5, time.time() + 1))
        time.sleep(1)
        t1 = time.monotonic()
        with view.lock:
            w = view.window
            p50, p90, p99 = load.percentiles([lat for _, lat in w['lat']])
            spr = len(w['sessions']) / w['placed'] if w['placed'] else None
            cpu = load.cpu_window(list(view.sampler.samples), r0.pid, t0, t1)
            cpu_per = cpu[0] / w['readable'] if cpu and w['readable'] else None
        time.sleep(HOLDS['live'])

        held = len(self.records)
        r1name = f'{prefix}-r1'
        with view.lock:
            view.catchup = {'name': r1name, 'target': held, 't0': None, 'state': 'running', 'evidence': None}
        view.set_phase(4)
        time.sleep(1)
        self.play(self.member(r0, r0, 'left', time.time() + 0.2))
        time.sleep(1.5)
        self.members.remove(r0)
        self.live.remove(r0)
        time.sleep(1)
        r1 = self.add(r1name)
        self.members.append(r1)
        self.play(self.member(nodes[0], r1, 'joined', time.time() + 0.05))
        start = time.time()
        if args.rehearse == 'hang':
            time.sleep(JOIN_BOUND_S + SILENT_S)
            state, catchup_s, rate = 'hung', None, None
            with view.lock:
                view.catchup['state'] = state
        else:
            self.play([(start + 0.03 + k / 1_000, r1, {
                'store': 'records', 'kind': 'insert_remote', 'key': key, 'entry_ts': ts, 'hash': h, 'len': S,
                'from': w.node_id, 'content_status': 'complete'}) for k, (key, h, ts, w) in enumerate(self.records)])
            with view.lock:
                view.catchup['state'] = state = 'done'
                catchup_s = view.catchup['last'] - view.catchup['t0']
            rate = held / catchup_s
        time.sleep(HOLDS['catchup'])

        with view.lock:
            view.result = {'p50': p50, 'p90': p90, 'p99': p99, 'cpu': cpu_per, 'spr': spr, 'catchup_s': catchup_s,
                           'catchup_rate': rate, 'held': held, 'state': state, 'evidence': view.catchup['evidence']}
        view.set_phase(5)

    def stop(self):
        pass


# --- drawing ---------------------------------------------------------------------------------------------------------

ANSI = re.compile(r'\x1b\[[0-9;?]*[A-Za-z]')


class Paint:
    def __init__(self, color):
        self.color = color

    def __call__(self, text, sgr):
        return f'\x1b[{sgr}m{text}\x1b[0m' if self.color and sgr else text


def bar(frac, width, p, sgr):
    filled = max(0, min(width, round(frac * width)))
    return p('█' * filled, sgr) + '░' * (width - filled)


def spread(left, right, width):
    return left + ' ' * max(1, width - len(left) - len(right)) + right


def cpu_now(samples, pid):
    pts = [(t, procs[pid][0]) for t, _, procs in list(samples)[-4:] if pid in procs]
    if len(pts) < 2 or pts[-1][0] <= pts[0][0]:
        return None
    return 100 * (pts[-1][1] - pts[0][1]) / (pts[-1][0] - pts[0][0])


def render(view, color, cols, rows):
    p = Paint(color)
    a = view.args
    width = max(72, min(cols, 120))
    out = []
    mode = 'REHEARSAL, synthetic data' if a.rehearse else 'LIVE'
    head = spread(' PDN pod sync — live load test',
                  f'{a.n} nodes · {a.v:,} records prefill · {S} B · {mode} ', width)
    out.append(p(head, '1;97;44'))
    if cols < 80 or rows < 30:
        out.append(p(f' terminal is {cols}×{rows}: enlarge it to at least 100×32 ', '1;30;43'))
    out.append('')
    chips = []
    for i, name in enumerate(PHASES):
        if i < view.phase:
            chips.append(p(f'✓ {name}', '1;32'))
        elif i == view.phase:
            chips.append(p(f' ✗ {name} ', '1;97;41') if view.error else p(f' ▶ {name} ', '1;30;43'))
        else:
            chips.append(name)
    out.append('  ' + p('  ›  ', '90').join(chips))
    elapsed = time.monotonic() - view.phase_t0
    clock = f'{elapsed:4.0f} / {a.steady_s:g} s' if view.phase == 3 else f'{elapsed:4.0f} s'
    note = textwrap.wrap(view.note, width - 16) or ['']
    out.append('  ' + p(spread(note[0], clock, width - 4), '1'))
    out += ['  ' + p(ln, '1') for ln in note[1:]]
    out.append(p('─' * width, '90'))

    if view.error:
        out.append('')
        out.append('  ' + p(' STOPPED ', '1;97;41') + '  ' + p('the run could not go on:', '1;91'))
        out += ['    ' + p(ln, '91') for ln in textwrap.wrap(view.error, width - 6)[:6]]
        out.append('')
        out.append('  ' + p('Fall back to the rehearsal:  python3 -I sync-test/demo.py --rehearse', '1;93'))
        if view.run_dir:
            out.append(f'  Logs and events: {view.run_dir}')
        out.append('  Press Enter to leave.')
        return out
    if view.phase == 5 and view.result:
        return out + result_lines(view, p, width)

    columns = f'{"CPU, share of one core":<38}{"memory":>10}{"records held":>15}'
    out.append(p(f' {"NODES":<24}', '1;96') + p(columns, '1'))
    for name, pid in view.roster():
        node = view.nodes.get(name, {})
        lab = label(name)
        role = 'writer' if lab != 'r' else 'reader' if name.endswith('-r0') else 'reader, rejoined'
        flash = time.monotonic() - node.get('flash', 0) < 0.5
        dot = p('●', '1;92') if flash else p('·', '90')
        cpu = cpu_now(view.sampler.samples, pid) if view.sampler else None
        rss = load.Sampler.rss_mb(view.sampler, pid) if view.sampler else None
        cpu_txt = '   –  ' if cpu is None else f'{cpu:5.0f} %'
        rss_txt = '    – ' if rss is None else f'{rss:4.0f} MB'
        out.append(f'  {p(f"{lab:<3}", "1;97")}{dot} {role:<17} {bar((cpu or 0) / 100, 30, p, "32")} {cpu_txt}'
                   f'   {rss_txt}   {p(f"{len(node.get("held", ())):>12,}", "1;97")}')
    out.append(p('─' * width, '90'))
    out += sync_lines(view, p, width)
    if view.catchup:
        out.append(p('─' * width, '90'))
        out += catchup_lines(view, p, width)
    out.append(p('─' * width, '90'))
    out.append(p(' WHAT IS HAPPENING', '1;96'))
    for at, text, style in list(view.ticker):
        out.append(f'  {p(at, "90")}  {p(text, style)}')
    return out


def sync_lines(view, p, width):
    a, w = view.args, view.window
    out = [p(' LIVE SYNC', '1;96')]
    if view.phase < 2 or not w:
        out.append('  waiting for the pod to form')
        return out
    if view.phase == 2:
        readable = w['readable']
        out.append(f'  Prefill   placed {p(f"{w["placed"]:,}", "1;97")} of {a.v:,}     '
                   f'readable on r {p(f"{readable:,}", "1;97")} of {a.v:,}')
        out.append('  ' + bar(readable / a.v if a.v else 1, 60, p, '1;36'))
        return out
    span = 'last 10 s' if view.phase == 3 else 'whole run'
    since = time.time() - 10 if view.phase == 3 else 0
    p50, p90, _ = load.percentiles([lat for t, lat in w['lat'] if t >= since])
    ok = w['readable'] + a.rate >= w['placed']
    out.append(f'  records placed {p(f"{w["placed"]:>7,}", "1;97")}        readable on r '
               f'{p(f"{w["readable"]:>7,}", "1;92" if ok else "1;93")}')
    out.append(f'  write → readable on r, {span}:   p50 {p(f"{ms(p50):>7}", "1;97;42")}     '
               f'p90 {p(f"{ms(p90):>7}", "1;97")}')
    if w['lat']:
        t0 = w['t0_us'] / 1e6
        buckets = collections.defaultdict(list)
        for t, lat in w['lat']:
            buckets[int(t - t0)].append(lat)
        series = [statistics.median(buckets[i]) if i in buckets else None for i in range(max(buckets) + 1)]
        top = max(x for x in series if x is not None)
        spark = ''.join(' ' if x is None else SPARK[max(1, round(x / top * 8))] for x in series)[-(width - 40):]
        out.append(f'  p50 each second  {p(spark, "1;96")}  0–{top * 1000:.0f} ms')
    if w['placed']:
        out.append(f'  sessions per record  {p(f"{len(w["sessions"]) / w["placed"]:.1f}", "1;97")}'
                   f'   ({len(w["sessions"]):,} sessions carried records)')
    return out


def catchup_lines(view, p, width):
    c = view.catchup
    out = [p(' CATCH-UP', '1;96') + '  r rejoins with an empty directory']
    held, target = view.held(c['name']), c['target']
    if c['state'] == 'hung':
        out.append('  ' + p(' join did not answer ', '1;97;41') + '  '
                   + p(f'r holds {held:,} of {target:,} records', '1;91'))
        where = f'evidence saved to {c["evidence"]}' if c['evidence'] else 'rehearsal: no evidence is saved'
        out.append('  ' + p(where, '91'))
        out.append('  ' + p('a finding, not a crash: the platform stops here' + (', and the run kept the evidence'
                                                                                  if c['evidence'] else ''), '1'))
        return out
    if c['t0'] is None:
        out.append('  r leaves the pod; its directory is wiped')
        return out
    secs = max(1e-3, (c['last'] if held >= target and c.get('last') else time.time()) - c['t0'])
    if held == 0:
        style = '1;93' if secs > 5 else '1'
        text = (f'join called, waiting for its first session   {secs:.0f} s of {JOIN_BOUND_S} s' if secs <= JOIN_BOUND_S
                else f'no answer after {JOIN_BOUND_S} s; checking whether r has gone silent   {secs:.0f} s')
        out.append('  ' + p(text, style))
        return out
    rate = held / secs if secs > 0 else 0
    out.append(f'  {bar(held / target if target else 1, 50, p, "1;92")}  {p(f"{held:,}", "1;97")} / {target:,} records')
    out.append(f'  {p(f"{rate:,.0f} records/s", "1;97")}   {secs:.2f} s since the join call'
               + (p('   incomplete at the deadline', '1;93') if c['state'] == 'incomplete' else ''))
    return out


def result_lines(view, p, width):
    a, r = view.args, view.result
    out = ['', p(f' RESULT', '1;96') + p(f'   {a.n} nodes on one laptop, {r["held"]:,} records of {S} B', '1'), '']
    lat = '    '.join(f'{q} {p(f"{ms(r[q]):>7}", "1;97")}' for q in ('p50', 'p90', 'p99'))
    cpu = '–' if r['cpu'] is None else f'{r["cpu"] * 1000:.0f} ms of CPU time'
    rows = [('Write → readable on r', lat), ('Reader CPU per record', p(cpu, '1;97'))]
    if r['state'] == 'done' and r['catchup_s']:
        rows.append(('Catch-up of a wiped reader', p(f'{r["held"]:,} records in {r["catchup_s"]:.2f} s', '1;97')
                     + f'  =  {p(f"{r["catchup_rate"]:,.0f} records/s", "1;97")}'))
    elif r['state'] == 'hung':
        rows.append(('Catch-up of a wiped reader', p(' join did not answer ', '1;97;41')))
    else:
        rows.append(('Catch-up of a wiped reader', p('incomplete at the deadline', '1;93')))
    rows.append(('Sessions per record', p('–' if r['spr'] is None else f'{r["spr"]:.1f}', '1;97')))
    for name, val in rows:
        out.append(f'   {name:<30}{val}')
        out.append('')
    if r['p50'] is None:
        sentence = 'No record of the live writes became readable on the reader.'
    else:
        sentence = (f'A record written on any writer was readable on the reader within about {r["p50"] * 1000:.0f} ms; '
                    f'99 records in 100 within {r["p99"] * 1000:.0f} ms.')
    if r['state'] == 'done' and r['catchup_s']:
        sentence += (f' A reader that lost its whole directory got all {r["held"]:,} records back in '
                     f'{r["catchup_s"]:.2f} s.')
    elif r['state'] == 'hung':
        sentence += f' The wiped reader\'s join of {r["held"]:,} records did not answer: a finding, not a crash.'
        sentence += f' The evidence is saved to {r["evidence"]}.' if r['evidence'] else ''
    out += ['   ' + p(ln, '1;93') for ln in textwrap.wrap(sentence, width - 6)]
    out.append('')
    if view.run_dir:
        out.append(f'   Numbers to re-check: {view.run_dir}  (load.csv, meta.json, events/)')
    out.append('   Press Enter to leave.')
    return out


# --- the screen ------------------------------------------------------------------------------------------------------

class Screen:
    """Alternate screen, hidden cursor, no autowrap, keys without echo; all restored on any exit."""

    def __enter__(self):
        self.fd = sys.stdin.fileno() if sys.stdin.isatty() else None
        self.saved = termios.tcgetattr(self.fd) if self.fd is not None else None
        if self.fd is not None:
            tty.setcbreak(self.fd)
        sys.stdout.write('\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[2J')
        sys.stdout.flush()
        return self

    def __exit__(self, *exc):
        sys.stdout.write('\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l')
        sys.stdout.flush()
        if self.saved is not None:
            termios.tcsetattr(self.fd, termios.TCSADRAIN, self.saved)

    def draw(self, lines):
        sys.stdout.write('\x1b[H' + ''.join(ln + '\x1b[K\n' for ln in lines) + '\x1b[J')
        sys.stdout.flush()

    def key(self, timeout):
        """The keys pressed within `timeout`, or ''."""
        if self.fd is None:
            time.sleep(timeout)
            return ''
        ready, _, _ = select.select([self.fd], [], [], timeout)
        return os.read(self.fd, 64).decode(errors='replace') if ready else ''


def guarded(drive, view):
    """Any failure of the run is drawn on the dashboard rather than freezing it; SystemExit is load.py's refusal."""
    try:
        drive()
    except (Exception, SystemExit) as e:
        with view.lock:
            view.error = f'{type(e).__name__}: {e}'


def interrupt(signum, frame):
    raise KeyboardInterrupt


def main():
    global VIEW
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--n', type=int, default=4, help='nodes: N-1 writers and the reader r')
    ap.add_argument('--v', type=int, default=1_000, help='records placed before the live writes')
    ap.add_argument('--steady-s', type=float, default=45)
    ap.add_argument('--rate', type=int, default=10, help='records a second during the live writes')
    ap.add_argument('--rehearse', nargs='?', const='ok', choices=['ok', 'hang'],
                    help='synthetic events, no node; =hang rehearses a join that does not answer')
    ap.add_argument('--no-color', action='store_true')
    ap.add_argument('--bin', default=str(load.DEFAULT_BIN))
    args = ap.parse_args()
    if args.n < 2 or args.v < 1 or args.rate < 1 or args.steady_s <= 0:
        ap.error('--n 2 or more; --v, --rate and --steady-s above 0')
    color = not args.no_color and 'NO_COLOR' not in os.environ
    frames = os.environ.get('DEMO_FRAMES')
    VIEW = view = View(args)
    world = Rehearsal(args, view) if args.rehearse else Live(args, view)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupt)
    interrupted = False
    try:
        with Screen() as screen:
            threading.Thread(target=guarded, args=(world.drive, view), daemon=True).start()
            last_frame, finished_at = 0.0, None
            while True:
                cols, rows = shutil.get_terminal_size((100, 34))
                with view.lock:
                    lines = render(view, color, cols, rows)
                screen.draw(lines)
                if frames and time.monotonic() - last_frame >= 1:
                    last_frame = time.monotonic()
                    with open(frames, 'a') as f:
                        f.write(f'=== {time.strftime("%H:%M:%S")} phase {PHASES[view.phase]} ===\n'
                                + '\n'.join(ANSI.sub('', ln).rstrip() for ln in lines) + '\n')
                keys = screen.key(0.25)
                over = view.result is not None or view.error is not None
                if over and ('\n' in keys or '\r' in keys or 'q' in keys):
                    break
                if over and screen.fd is None:
                    finished_at = finished_at or time.monotonic()
                    if time.monotonic() - finished_at > 3:
                        break
    except KeyboardInterrupt:
        interrupted = True
    finally:
        world.stop()
    if view.run_dir:
        print(f'run directory: {view.run_dir}')
    if interrupted:
        print('interrupted; every node this demo started is stopped')
        return 130
    return 1 if view.error else 0


if __name__ == '__main__':
    sys.exit(main())
