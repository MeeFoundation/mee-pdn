#!/usr/bin/env -S python3 -I
"""Runs load.md's test L, plain variant, and test P: per N and S one pod of N pdn-node-http processes — in test P
(`--variant phone`, through pdn-app's phone.py) N − 1 of them and the phone's test build as r — filled level by level of V,
each cell running idle, steady and catch-up; appends one row per cell to load.csv in the run directory.

The rules are load.md's: the matrix, the workloads, "What is measured, and how", the marks of "A cell's validity", "When
the platform cannot finish a cell" and "Running the matrix unattended". The columns are plan.md's Step 3L, read by
analyze.py. `--selfcheck` checks the metric computations on synthetic events.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import collections
import csv
import gzip
import http.client
import importlib.util
import json
import os
import random
import shutil
import signal
import socket
import statistics
import subprocess
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# Test P's phone: in the application repository nested in the workspace.
PHONE = ROOT / 'pdn-app' / 'sync-test' / 'phone.py'
DEFAULT_BIN = ROOT / 'target' / 'release' / 'pdn-node-http'
LINUX = sys.platform.startswith('linux')

_phone = None


def phone_module():
    """pdn-app's phone.py, loaded for test P alone: the laptop-only runner, CI's image included, carries no phone
    code."""
    global _phone
    if _phone is None:
        spec = importlib.util.spec_from_file_location('phone', PHONE)
        _phone = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(_phone)
    return _phone

QUIET_PCT, QUIET_S, QUIET_BOUND_S = 5.0, 10, 120
IDLE_S = 60
# The first repeat's idle holds one whole pod interval: 330 s at the runtime's default of 300.
LONG_IDLE_EXTRA_S = 30
DEFAULT_POD_INTERVAL_S = 300
STEADY_S, STEADY_RATE, DRAIN_S = 60, 10, 60
CATCHUP_MIN_S, CATCHUP_RECORDS_PER_S = 120, 100
# After a lost event the joiner's listing is read this often: the catch-up's end is then late by up to this much.
CATCHUP_LISTING_EVERY_S = 1
# --grid: a fresh pod per (N, V) and short windows, to map where sync stays stable; no idle verdicts.
GRID_FILL_STALL_S = 60
FILL_MIN_S, FILL_RECORDS_PER_S = 300, 20
SILENT_S = 30
STATUS_S = 30
LEAVE_S = 60
CONTENDED_SHARE = 0.7
SLEEP_JUMP_S = 5
THERMAL_WAIT_S = 600
EVIDENCE_EVENTS = 200
SIZES_64K_MAX_V = 10_000
KIND = 'immutable-document'

COLUMNS = ['run', 'test', 'variant', 'N', 'V', 'V_actual', 'S', 'repeat', 'contended', 'lagged', 'incomplete',
           'join_hung', 'not_run', 'inactive', 'low_power', 'slept', 'not_quiet', 'thermal', 'pod_pass', 'evidence',
           'steady_p50_s', 'steady_p90_s', 'steady_p99_s', 'catchup_s', 'catchup_records_per_s',
           'r_cpu_s_per_record', 'sessions_per_record', 'idle_r_cpu_pct', 'idle_long_r_cpu_pct', 'r_rss_mb',
           'observation_cpu_s']


def now_us():
    return int(time.time() * 1_000_000)


class NodeDown(Exception):
    """A node did not answer within its bound: the cell is incomplete and its N restarts."""


# --- pure parts, covered by --selfcheck ------------------------------------------------------------------------------

def cpu_budget():
    """The CPUs this run may use: a container's cgroup quota or affinity on Linux, the cores on macOS."""
    if LINUX:
        try:
            quota, period = Path('/sys/fs/cgroup/cpu.max').read_text().split()
            if quota != 'max':
                return round(int(quota) / int(period), 2)
        except (OSError, ValueError):
            pass
        return len(os.sched_getaffinity(0))
    return int(subprocess.check_output(['sysctl', '-n', 'hw.ncpu']).decode())


def proc_sample(pids):
    """{pid: (CPU seconds, RSS in KiB)} of the live pids: /proc on Linux (10 ms ticks), `ps` elsewhere."""
    if LINUX:
        tick, page = os.sysconf('SC_CLK_TCK'), os.sysconf('SC_PAGE_SIZE')
        out = {}
        for pid in pids:
            try:
                stat = Path(f'/proc/{pid}/stat').read_text()
                rss_pages = int(Path(f'/proc/{pid}/statm').read_text().split()[1])
            except (OSError, IndexError, ValueError):
                continue
            # Fields after the parenthesised command name, which may itself hold spaces.
            fields = stat.rsplit(')', 1)[1].split()
            out[pid] = ((int(fields[11]) + int(fields[12])) / tick, rss_pages * page // 1024)
        return out
    res = subprocess.run(['ps', '-o', 'pid=,cputime=,rss=', '-p', ','.join(map(str, pids))],
                         capture_output=True, text=True)
    out = {}
    for line in res.stdout.splitlines():
        pid, cpu, rss = line.split()
        out[int(pid)] = (cputime_s(cpu), int(rss))
    return out


def host_info():
    """What the numbers were measured on, and the hypervisor's steal so far (Linux), for meta.json."""
    info = {'platform': sys.platform, 'cpu_budget': cpu_budget(), 'cpu_count': os.cpu_count()}
    if LINUX:
        try:
            info['cpu_model'] = next(l.split(':', 1)[1].strip() for l in Path('/proc/cpuinfo').read_text().splitlines()
                                     if l.startswith(('model name', 'Model')))
        except (OSError, StopIteration):
            pass
        try:
            cpu = Path('/proc/stat').read_text().splitlines()[0].split()
            info['steal_s'] = int(cpu[8]) / os.sysconf('SC_CLK_TCK')
        except (OSError, IndexError, ValueError):
            pass
    else:
        info['cpu_model'] = subprocess.run(['sysctl', '-n', 'machdep.cpu.brand_string'], capture_output=True,
                                           text=True).stdout.strip()
    return info


def cputime_s(text):
    """`ps -o cputime=`: [[D-]H:]M:SS.ss as seconds."""
    days, _, rest = text.strip().rpartition('-')
    secs = 0.0
    for part in rest.split(':'):
        secs = secs * 60 + float(part)
    return secs + (int(days) * 86_400 if days else 0)


def percentiles(xs):
    """(p50, p90, p99), or Nones without a value."""
    if not xs:
        return None, None, None
    if len(xs) == 1:
        return xs[0], xs[0], xs[0]
    q = statistics.quantiles(xs, n=100, method='inclusive')
    return q[49], q[89], q[98]


def is_gap(prev, rec):
    """Events were lost between prev and rec: `lagged`, a new `sub`, or a `seq` that does not follow."""
    if rec['kind'] == 'lagged':
        return True
    if prev is None:
        return rec['seq'] != 0
    return rec['sub'] != prev['sub'] or rec['seq'] != prev['seq'] + 1


class Stream:
    """One node's event stream: every record appended to its events file with the receive time `rt`, and what the
    cells read from it. `track` keeps the per-record maps, which only `r`'s stream needs."""

    def __init__(self, path, track, offset_us=0):
        # Line-buffered: the file is the evidence of what the stream delivered, up to the last record.
        self.file = path.open('a', buffering=1) if path else None
        self.track = track
        # The node's clock minus the laptop's: a record's readable time is kept on the laptop's clock.
        self.offset_us = offset_us
        self.prev = None
        self.last = None     # monotonic time of the last record
        self.count = 0
        self.gaps = 0
        self.ready = {}      # records-store hash -> t it became readable
        self.entry_ts = {}   # records-store hash -> the entry's own timestamp
        self.order = []      # records-store hashes in insert_remote order
        self.sessions = []   # (store, started, finished, session, received, origin) per sync_finished
        self.tail = collections.deque(maxlen=EVIDENCE_EVENTS)
        self.answered = threading.Event()
        self.answered_at = None
        self.status = None
        self.error = None
        self.ended = False
        self.conn = None

    def feed(self, line):
        if self.file:
            self.tail.append(line)
        rec = json.loads(line)
        self.last = time.monotonic()
        self.count += 1
        if self.file:
            rec['rt'] = now_us()
            self.file.write(json.dumps(rec, separators=(',', ':')) + '\n')
        self.gaps += is_gap(self.prev, rec)
        self.prev = rec
        kind = rec['kind']
        if kind == 'sync_finished':
            self.sessions.append((rec['store'], rec['started'] - self.offset_us, rec['finished'] - self.offset_us,
                                  rec.get('session'), rec.get('received') or 0, rec['origin']))
        if not self.track or rec['store'] != 'records':
            return
        if kind == 'insert_remote':
            self.entry_ts[rec['hash']] = rec['entry_ts']
            self.order.append(rec['hash'])
            if rec['content_status'] == 'complete':
                self.ready.setdefault(rec['hash'], rec['t'] - self.offset_us)
        elif kind == 'content_ready':
            self.ready.setdefault(rec['hash'], rec['t'] - self.offset_us)

    def held_since(self, mark):
        return sum(1 for h in self.order[mark:] if h in self.ready)

    def latencies_s(self, mark):
        """Placement on the writer to readable here, per record that arrived after `mark`."""
        return [(self.ready[h] - self.entry_ts[h]) / 1e6 for h in self.order[mark:] if h in self.ready]

    def close(self):
        if self.conn and self.conn.sock:
            try:
                self.conn.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        if self.file:
            self.file.flush()


def sessions_carrying(streams, t0_us, t1_us):
    """Distinct record-store sessions that finished in the window with `received` above 0: each side reports one,
    and it counts once."""
    return len({s for st in streams for (store, _, fin, s, rec, _) in st.sessions
                if store == 'records' and s and rec > 0 and t0_us <= fin <= t1_us})


def cpu_window(samples, pid, t0, t1):
    """(CPU seconds, average % of one core, peak % of one core) of `pid` from the last sample at or before t0 to the
    first at or after t1, or None: a window shorter than the sampling period still reads."""
    all_pts = [(t, procs[pid][0]) for t, _, procs in samples if pid in procs]
    before = [i for i, (t, _) in enumerate(all_pts) if t <= t0]
    after = [i for i, (t, _) in enumerate(all_pts) if t >= t1]
    pts = all_pts[before[-1] if before else 0:(after[0] if after else len(all_pts) - 1) + 1]
    if len(pts) < 2 or pts[-1][0] <= pts[0][0]:
        return None
    cpu = pts[-1][1] - pts[0][1]
    peak = max(100 * (c1 - c0) / (b - a) for (a, c0), (b, c1) in zip(pts, pts[1:]) if b > a)
    return cpu, 100 * cpu / (pts[-1][0] - pts[0][0]), peak


def predicted_passes(start, interval, t0, t1):
    """A node's periodic pod passes inside [t0, t1]: one every interval from the node's start."""
    k = max(1, int((t0 - start) // interval))
    out = []
    while start + k * interval <= t1:
        if start + k * interval >= t0:
            out.append(start + k * interval)
        k += 1
    return out


def selfcheck():
    assert listed_complete([{'present': True}] * 3, 3), 'every record held with its payload'
    assert not listed_complete([{'present': True}, {'present': False}, {'present': True}], 3), 'a payload missing'
    assert cputime_s('0:01.50') == 1.5 and cputime_s('1:02:03.00') == 3723 and cputime_s('1-00:00:01.00') == 86_401
    assert percentiles([]) == (None, None, None) and percentiles([2.0]) == (2.0, 2.0, 2.0)
    assert percentiles([float(i) for i in range(101)])[:2] == (50.0, 90.0)
    st = Stream(None, track=True)
    base = {'sub': 'a', 'pod': 'p'}
    events = [
        {'seq': 0, 'kind': 'insert_remote', 'store': 'membership', 'key': 'm', 'entry_ts': 1, 'hash': 'm1', 'len': 1,
         'from': 'x', 'content_status': 'complete', 't': 5},
        {'seq': 1, 'kind': 'insert_remote', 'store': 'records', 'key': 'k1', 'entry_ts': 1_000_000, 'hash': 'h1',
         'len': 256, 'from': 'x', 'content_status': 'complete', 't': 1_500_000},
        {'seq': 2, 'kind': 'insert_remote', 'store': 'records', 'key': 'k2', 'entry_ts': 2_000_000, 'hash': 'h2',
         'len': 256, 'from': 'x', 'content_status': 'missing', 't': 2_100_000},
        {'seq': 3, 'kind': 'content_ready', 'store': 'records', 'hash': 'h2', 't': 4_000_000},
        {'seq': 5, 'kind': 'sync_finished', 'store': 'records', 'peer': 'x', 'origin': 'accept', 'session': 's1',
         'started': 1, 'finished': 3_000_000, 'ok': True, 'sent': 0, 'received': 2, 't': 3_000_000},
        {'seq': 6, 'kind': 'insert_remote', 'store': 'records', 'key': 'k3', 'entry_ts': 3_000_000, 'hash': 'h3',
         'len': 256, 'from': 'x', 'content_status': 'missing', 't': 3_100_000},
    ]
    for e in events:
        st.feed(json.dumps(base | e))
    assert st.gaps == 1, 'the seq gap 3 -> 5 is one loss'
    assert st.held_since(0) == 2 and st.held_since(2) == 0, 'h3 has no content yet; membership never counts'
    assert sorted(st.latencies_s(0)) == [0.5, 2.0]
    other = Stream(None, track=False)
    for i, e in enumerate([{'session': 's1', 'received': 0}, {'session': 's2', 'received': 1},
                           {'session': 's3', 'received': 4, 'store': 'membership'},
                           {'session': 's4', 'received': 1, 'finished': 9_000_000}]):
        other.feed(json.dumps({'sub': 'b', 'seq': i, 'kind': 'sync_finished', 'store': 'records', 'peer': 'y',
                               'origin': 'connect', 'started': 1, 'finished': 2_000_000, 'ok': True, 't': 1} | e))
    assert other.gaps == 0 and sessions_carrying([st, other], 0, 5_000_000) == 2, 's1 once, s2; not s3, s4'
    st.feed(json.dumps(base | {'seq': 7, 'kind': 'lagged', 'store': 'records', 't': 1}))
    assert st.gaps == 2
    samples = [(0.0, 0, {7: (1.0, 0)}), (1.0, 0, {7: (1.5, 0)}), (2.0, 0, {7: (1.6, 0)})]
    cpu, avg, peak = cpu_window(samples, 7, 0, 2)
    assert abs(cpu - 0.6) < 1e-9 and abs(avg - 30) < 1e-9 and abs(peak - 50) < 1e-9
    assert cpu_window(samples, 7, 0.4, 0.6)[0] == 0.5, 'a short window reads from the samples around it'
    assert predicted_passes(100, 300, 350, 1000) == [400, 700, 1000] and predicted_passes(100, 300, 0, 300) == []
    assert record_id('by/m1/immutable-document/014bac/1') == '014bac' and record_id('membership') is None
    ahead = Stream(None, track=True, offset_us=500_000)
    ahead.feed(json.dumps(base | events[1] | {'seq': 0, 'content_status': 'missing'}))
    ahead.feed(json.dumps(base | events[3] | {'seq': 1, 'hash': 'h1'}))
    assert ahead.latencies_s(0) == [2.5], 'a phone 0.5 s ahead: readable at 3.5 s on the laptop, placed at 1 s'
    sampler = Sampler.__new__(Sampler)
    sampler.phone_points = collections.deque()
    device = {'kind': 'cpu', 'app_state': 'active', 'low_power': False, 'resident_bytes': 2 << 20, 'cpu_user_us': 0,
              'cpu_sys_us': 0, 'node_cpu_sys_us': 0}
    for t, node_us, kind, state in [(0, 0, 'cpu', 'active'), (1, 400_000, 'cpu', 'active'),
                                    (1.5, 400_000, 'state', 'inactive'), (2, 500_000, 'cpu', 'inactive')]:
        sampler.phone_points.append((t, device | {'kind': kind, 'app_state': state, 'node_cpu_user_us': node_us}))
    cpu, _, peak = cpu_window(sampler.phone_series(lambda s: s['node_cpu_user_us']), PhoneNode.PID, 0, 2)
    assert abs(cpu - 0.5) < 1e-9 and abs(peak - 40) < 1e-9, 'the state sample is no CPU point'
    assert sampler.phone_marks(0, 1) == {'inactive': '0', 'low_power': '0'}
    assert sampler.phone_marks(0, 30)['inactive'] == '1', '3 samples in 30 s: the application was not sampling'
    assert sampler.phone_marks(0, 1.6)['inactive'] == '1', 'a change of state inside the window marks it'
    assert sampler.rss_mb(PhoneNode.PID) == 2.0
    print('selfcheck: PASS')
    return 0


# --- nodes and HTTP --------------------------------------------------------------------------------------------------

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def request(port, method, path, body=None, timeout=STATUS_S, conn=None, raw=False):
    """One request; the answer as JSON (or bytes with `raw`). A refusal raises RuntimeError, silence NodeDown."""
    c = conn or http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
    if isinstance(body, (dict, list, str)):
        body, ctype = json.dumps(body).encode(), 'application/json'
    else:
        ctype = 'application/octet-stream'
    try:
        c.request(method, path, body=body, headers={'content-type': ctype} if body is not None else {})
        resp = c.getresponse()
        data = resp.read()
    except (socket.timeout, ConnectionError, http.client.HTTPException, OSError) as e:
        c.close()
        raise NodeDown(f'{method} {path} on :{port}: {e!r}') from e
    finally:
        if conn is None:
            c.close()
    if resp.status >= 300:
        raise RuntimeError(f'{method} {path} on :{port}: {resp.status} {data[:300]!r}')
    if raw or not data:
        return data
    return json.loads(data)


class Node:
    offset_us = 0

    def __init__(self, run, name):
        self.run, self.name = run, name
        self.data = run.nodes / name
        self.log = run.dir / 'logs' / f'{name}.log'
        self.port = free_port()
        self.proc = None
        self.identity = None
        self.stream = None
        self.started = None

    @property
    def pid(self):
        return self.proc.pid

    def start(self):
        self.data.mkdir(parents=True)
        self.log.parent.mkdir(exist_ok=True)
        env = {**os.environ, 'PDN_DATA_DIR': str(self.data), 'PDN_HOST': '127.0.0.1', 'PDN_PORT': str(self.port),
               'PDN_DEBUG': '1', 'PDN_CONNECTIVITY': 'direct'}
        if self.run.phone:
            # The phone dials the writers at the local network addresses their invites carry.
            env.pop('PDN_BIND_ADDR', None)
        if self.run.meta['pod_interval_set']:
            env['PDN_POD_RECONCILE_INTERVAL_SECS'] = str(self.run.meta['pod_interval_s'])
        with self.log.open('ab') as log:
            self.proc = subprocess.Popen([str(self.run.bin)], env=env, stdout=log, stderr=subprocess.STDOUT,
                                         stdin=subprocess.DEVNULL, start_new_session=True)
        self.started = time.monotonic()
        self.run.record_pid(self)
        self.run.live[self.pid] = self
        deadline = time.monotonic() + STATUS_S
        while True:
            try:
                status = self.status()
                break
            except (NodeDown, RuntimeError):
                if time.monotonic() > deadline or self.proc.poll() is not None:
                    raise NodeDown(f'{self.name} did not answer its status; see {self.log}')
                time.sleep(0.2)
        if 'profile release' not in status or 'observation on' not in status:
            self.stop()
            sys.exit(f'{self.run.bin} answers {status!r}: a release build with the observation feature only')
        self.run.meta['profiles'] = sorted(set(self.run.meta.get('profiles', [])) | {
            ' '.join(line for line in status if line.startswith(('profile', 'observation')))})
        self.identity = request(self.port, 'POST', '/debug/identities')['identity']
        return self

    def status(self):
        return request(self.port, 'GET', '/debug/status', raw=True).decode().splitlines()

    def call(self, method, path, body=None, timeout=STATUS_S, raw=False):
        return request(self.port, method, f'/debug/identities/{self.identity}{path}', body, timeout, raw=raw)

    def open_stream(self, pod, track, join_invite=None):
        """The pod's event stream on this node, read by a thread of its own; with `join_invite`, the observed join's."""
        stream = Stream(self.run.dir / 'events' / f'{self.name}.jsonl', track, self.offset_us)

        def pump():
            try:
                stream.conn = http.client.HTTPConnection('127.0.0.1', self.port, timeout=None)
                if join_invite is None:
                    stream.conn.request('GET', f'/debug/identities/{self.identity}/pods/{pod}/events')
                else:
                    stream.conn.request('POST', f'/debug/identities/{self.identity}/pods/join-observed',
                                        body=json.dumps(join_invite).encode(),
                                        headers={'content-type': 'application/json'})
                resp = stream.conn.getresponse()
                stream.status, stream.answered_at = resp.status, time.monotonic()
                if resp.status != 200:
                    stream.error = resp.read()[:500].decode(errors='replace')
                    return
                stream.answered.set()
                for line in resp:
                    if line.strip():
                        stream.feed(line.decode())
            except (OSError, http.client.HTTPException, ValueError) as e:
                stream.error = stream.error or repr(e)
            finally:
                stream.ended = True
                stream.answered.set()
                if stream.file:
                    stream.file.flush()

        threading.Thread(target=pump, daemon=True).start()
        self.stream = stream
        return stream

    def stop(self):
        if self.proc is None or self.proc.poll() is not None:
            return
        try:
            os.killpg(self.pid, signal.SIGTERM)
            self.proc.wait(15)
        except subprocess.TimeoutExpired:
            os.killpg(self.pid, signal.SIGKILL)
            self.proc.wait()
        except ProcessLookupError:
            pass
        if self.stream:
            self.stream.close()
        self.run.live.pop(self.pid, None)


class PhoneNode(Node):
    """`r` on the phone in test P: the test build's node behind its surface, brought up anew by a reset for each
    incarnation; its CPU and memory come from the device's samples."""
    PID = 'phone'

    def __init__(self, run, name):
        super().__init__(run, name)
        self.port = run.phone.port

    @property
    def pid(self):
        return self.PID

    def start(self):
        self.log.parent.mkdir(exist_ok=True)
        self.log.touch()
        try:
            self.offset_us = self.run.clock_offset()
            up = self.run.phone.reset(self.run.meta['pod_interval_s'])
        except (OSError, http.client.HTTPException, ValueError) as e:
            raise NodeDown(f'{self.name}: the phone did not answer: {e!r}') from e
        self.started = time.monotonic()
        status = self.status()
        if up['profile'] != 'release' or 'observation on' not in status:
            sys.exit(f'the phone answers {up["profile"]} and {status!r}: its test build in the release configuration '
                     'only')
        self.run.meta['profiles'] = sorted(set(self.run.meta.get('profiles', [])) | {
            'phone ' + ' '.join(line for line in status if line.startswith(('profile', 'observation')))})
        self.identity = up['identity']
        return self

    def stop(self):
        if self.stream:
            self.stream.close()


# --- the run ---------------------------------------------------------------------------------------------------------

class Sampler(threading.Thread):
    """Every second: `ps` CPU time and RSS of every live node, and whether the wall clock jumped against the monotonic
    one (the laptop slept)."""

    def __init__(self, run):
        super().__init__(daemon=True)
        self.owner = run
        self.samples = collections.deque(maxlen=4_000)
        self.jumps = []
        self.ncpu = cpu_budget()
        self.phone_points = collections.deque(maxlen=20_000)  # (monotonic t, the device's sample)
        self.phone_seq = None

    def run(self):
        offset = time.time() - time.monotonic()
        with (self.owner.dir / 'cpu.jsonl').open('a') as out:
            while True:
                pids = list(self.owner.live)
                procs = {}
                if pids:
                    procs = proc_sample(pids)
                t = time.monotonic()
                self.samples.append((t, time.time(), procs))
                out.write(json.dumps({'t': time.time(), 'cpu_s': {self.owner.live[p].name: c for p, (c, _) in procs.items()
                                                                 if p in self.owner.live}}) + '\n')
                out.flush()
                if self.owner.phone:
                    self.poll_phone()
                o = time.time() - time.monotonic()
                if abs(o - offset) > SLEEP_JUMP_S:
                    self.jumps.append(t)
                offset = o
                time.sleep(max(0.0, 1.0 - (time.monotonic() - t)))

    def poll_phone(self):
        """The device samples taken since the last poll, placed on the laptop's monotonic clock and kept in
        phone.jsonl; a silent phone is the requests' to report."""
        try:
            # Short: a silent phone must not stall the laptop's own samples.
            got = self.owner.phone.samples(self.phone_seq, timeout=2)
        except (OSError, http.client.HTTPException, RuntimeError, ValueError):
            return
        shift = self.owner.offset_us / 1e6 + time.time() - time.monotonic()
        with (self.owner.dir / 'phone.jsonl').open('a') as out:
            for seq, sample in got:
                self.phone_seq = seq
                self.phone_points.append((sample['t'] / 1e6 - shift, sample))
                out.write(json.dumps({'seq': seq, 'launch': self.owner.launches, **sample}) + '\n')

    def phone_series(self, field):
        """`field` of every periodic device sample as the samples' format: {PID: (CPU seconds, RSS KiB)}."""
        return [(t, None, {PhoneNode.PID: (field(s) / 1e6, s['resident_bytes'] // 1024)})
                for t, s in list(self.phone_points) if s['kind'] == 'cpu']

    def window(self, pid, t0, t1, field=None):
        """`cpu_window` of a laptop node, or of the phone's node threads (or of `field` of its samples)."""
        if pid != PhoneNode.PID:
            while self.samples and self.samples[-1][0] < t1 and time.monotonic() < t1 + 3:
                time.sleep(0.1)
            return cpu_window(list(self.samples), pid, t0, t1)
        while (not self.phone_points or self.phone_points[-1][0] < t1) and time.monotonic() < t1 + 3:
            time.sleep(0.1)
        node = lambda s: s['node_cpu_user_us'] + s['node_cpu_sys_us']
        return cpu_window(self.phone_series(field or node), pid, t0, t1)

    def phone_marks(self, t0, t1):
        """`inactive` and `low_power` of the device samples taken from t0 to t1; a window short of samples is
        `inactive` too, since a suspended application samples nothing."""
        inside = [s for t, s in list(self.phone_points) if t0 <= t <= t1]
        short = sum(s['kind'] == 'cpu' for s in inside) < 0.9 * (t1 - t0) - 2
        return {'inactive': '1' if short or any(s['kind'] == 'state' or s['app_state'] != 'active'
                                                for s in inside) else '0',
                'low_power': '1' if any(s['low_power'] for s in inside) else '0'}

    def thermal(self):
        latest = [s for _, s in list(self.phone_points) if s['kind'] == 'cpu'][-1:]
        return latest[0]['thermal'] if latest else None

    def host_share(self, t0, t1):
        """The nodes' total CPU over the window as a share of the CPUs the run may use."""
        self.window(None, t0, t1)
        samples = list(self.samples)
        pids = {p for t, _, procs in samples if t0 - 1 <= t <= t1 + 1 for p in procs}
        pcts = [w[1] for w in (cpu_window(samples, p, t0, t1) for p in pids) if w]
        return sum(pcts) / 100 / self.ncpu

    def rss_mb(self, pid):
        samples = self.phone_series(lambda s: 0) if pid == PhoneNode.PID else self.samples
        for _, _, procs in reversed(samples):
            if pid in procs:
                return procs[pid][1] / 1024
        return None


class Run:
    def __init__(self, args):
        self.dir = Path(args.out) / args.run_id
        self.nodes = Path(args.nodes_dir) / args.run_id if args.nodes_dir else self.dir / 'nodes'
        self.bin = Path(args.bin).resolve()
        self.live = {}
        self.phone = None
        self.offset_us = 0
        self.launches = 0
        self.csv = self.dir / 'load.csv'
        resumed = (self.dir / 'meta.json').exists()
        if resumed:
            self.meta = json.loads((self.dir / 'meta.json').read_text())
        else:
            ns = list(args.n)
            random.Random(args.run_id).shuffle(ns)
            self.meta = {'run': args.run_id, 'test': 'P' if args.variant == 'phone' else 'L', 'variant': args.variant,
                         'N': ns, 'V': args.v,
                         'S': args.s, 'repeats': args.repeats, 'obs_cost': args.obs_cost,
                         'pod_interval_s': args.pod_interval or DEFAULT_POD_INTERVAL_S,
                         'pod_interval_set': args.pod_interval is not None, 'bin': str(self.bin),
                         'cpu_cores': os.cpu_count(), 'revisions': revisions(), 'host': host_info(), 'cells': [], 'fills': [],
                         'obs_cost_runs': [], 'started': time.time()}
            (self.dir / 'events').mkdir(parents=True)
        if resumed:
            if self.meta['variant'] != args.variant:
                sys.exit(f'run {args.run_id} is a {self.meta["variant"]} run: resume it with --variant '
                         f'{self.meta["variant"]}')
            self.meta.setdefault('resumes', []).append(time.time())
        self.stop_recorded()
        self.preflight()
        self.done = self.resume_split() if resumed else set()
        if args.variant == 'phone':
            self.attach_phone()
        self.save()
        self.sampler = Sampler(self)
        self.meta['cpu_cores'] = self.sampler.ncpu
        self.sampler.start()

    def attach_phone(self):
        """The phone on the cable, its test build launched for this run; refuses a test build of another commit and a
        phone in Low Power Mode."""
        phone = phone_module()
        record = phone.installed_test_build()
        if record is None:
            sys.exit('preflight: no test build of the application was built here; python3 -I pdn-app/sync-test/phone.py build')
        head = subprocess.run(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout
        if record['mee_pdn_commit'] != head.strip():
            sys.exit(f'preflight: the phone\'s test build packages {record["mee_pdn_commit"][:12]}, the workspace '
                     f'is at {head.strip()[:12]}; python3 -I pdn-app/sync-test/phone.py build')
        if record['dirty'] or 'dirty' in self.meta['revisions'].get('workspace', ''):
            print('preflight: the workspace or the test build is dirty: the phone may run other code than the '
                  'laptop nodes', flush=True)
        self.phone = phone.Phone(phone.udid(), free_port())
        self.meta.setdefault('phone', {}).update(udid=self.phone.udid, test_build=record)
        self.meta['phone'].setdefault('offsets', [])
        self.phone.open_tunnel()
        try:
            self.check_phone()
        except BaseException:
            self.phone.close()
            raise

    def check_phone(self):
        self.launch()
        first = []
        deadline = time.monotonic() + 10
        while not first and time.monotonic() < deadline:
            time.sleep(1)
            first = [s for _, s in self.phone.samples() if s['kind'] == 'cpu']
        if not first:
            sys.exit('preflight: the phone hands over no device sample')
        if first[-1]['low_power']:
            sys.exit('preflight: Low Power Mode is on on the phone: it lowers CPU clocks and defers network work')
        if first[-1]['battery'] not in ('charging', 'full'):
            print(f'preflight: the phone\'s battery is {first[-1]["battery"]}, not charging', flush=True)

    def launch(self):
        """A fresh process of the test build: the samples count from 0 again and every subscription is new."""
        self.phone.launch(self.meta['run'])
        self.launches += 1
        self.meta['phone'].setdefault('launches', []).append(time.time())
        if hasattr(self, 'sampler'):
            self.sampler.phone_seq = None

    def clock_offset(self):
        """The phone's clock minus the laptop's, measured now through the tunnel, in µs."""
        offset, bound = self.phone.clock_offset_us()
        self.offset_us = offset
        self.meta['phone']['offsets'].append([round(time.time(), 1), offset, bound])
        return offset

    def reader(self, name):
        return PhoneNode(self, name) if self.phone else Node(self, name)

    def levels(self, s):
        return [v for v in self.meta['V'] if s == 256 or v <= SIZES_64K_MAX_V]

    def save(self):
        tmp = self.dir / 'meta.json.tmp'
        tmp.write_text(json.dumps(self.meta, indent=1))
        tmp.replace(self.dir / 'meta.json')

    def record_pid(self, node):
        with (self.dir / 'pids.jsonl').open('a') as f:
            f.write(json.dumps({'name': node.name, 'pid': node.pid, 'pgid': os.getpgid(node.pid)}) + '\n')

    def stop_recorded(self):
        """Stops the processes this run id started in an earlier coordinator, and only those."""
        path = self.dir / 'pids.jsonl'
        for line in path.read_text().splitlines() if path.exists() else []:
            rec = json.loads(line)
            comm = subprocess.run(['ps', '-o', 'comm=', '-p', str(rec['pid'])], capture_output=True, text=True)
            if comm.stdout.strip().endswith('pdn-node-http'):
                try:
                    os.killpg(rec['pgid'], signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def preflight(self):
        found = subprocess.run(['pgrep', '-lx', 'pdn-node-http'], capture_output=True, text=True).stdout.strip()
        if found:
            sys.exit(f'preflight: pdn-node-http processes this run did not start:\n{found}\nstop them first')
        if not self.bin.exists():
            sys.exit(f'preflight: no node binary at {self.bin}; cargo build --release -p pdn-node-http '
                     '--features observation')
        need = 2 * max(n * max(self.levels(s), default=0) * s for n in self.meta['N'] for s in self.meta['S'])
        free = shutil.disk_usage(self.dir).free
        if free < need:
            sys.exit(f'preflight: {free / 1e9:.1f} GB free, the run needs {need / 1e9:.1f} GB')
        if shutil.which('caffeinate'):
            self.caffeinate = subprocess.Popen(['caffeinate', '-dims', '-w', str(os.getpid())])

    def expected_rows(self):
        return sum(len(self.levels(s)) for s in self.meta['S']) * self.meta['repeats']

    def resume_split(self):
        """Keeps every N whose rows are all in load.csv; discards what an unfinished N left."""
        rows = list(csv.DictReader(self.csv.open())) if self.csv.exists() else []
        per_n = collections.Counter(int(r['N']) for r in rows)
        done = {n for n, c in per_n.items() if c >= self.expected_rows()}
        for n in set(self.meta['N']) - done:
            self.discard(n)
        return done

    def discard(self, n):
        """Moves N's rows to load.discarded.csv and sets its events aside, so N runs again from a fresh pod."""
        rows = list(csv.DictReader(self.csv.open())) if self.csv.exists() else []
        append_rows(self.dir / 'load.discarded.csv', [r for r in rows if int(r['N']) == n])
        self.csv.unlink(missing_ok=True)
        append_rows(self.csv, [r for r in rows if int(r['N']) != n])
        self.meta['cells'] = [c for c in self.meta['cells'] if c['N'] != n]
        for path in (self.dir / 'events').glob(f'n{n}s*.jsonl'):
            path.rename(path.with_suffix('.discarded'))
        for path in self.nodes.glob(f'n{n}s*'):
            shutil.rmtree(path, ignore_errors=True)

    def row(self, n, v, s, rep, held, **values):
        row = {c: '' for c in COLUMNS}
        row.update(run=self.meta['run'], test=self.meta['test'], variant=self.meta['variant'], N=n, V=v,
                   V_actual=held, S=s, repeat=rep)
        row.update(values)
        append_rows(self.csv, [row])
        return row

    def stop_all(self):
        """Every node this run started: SIGTERM to all at once, then SIGKILL to whatever is left after 15 s. Each
        node is its own process group, so one interrupted wait still leaves none behind."""
        nodes = list(self.live.values())
        for node in nodes:
            try:
                os.killpg(node.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + 15
        for node in nodes:
            try:
                node.proc.wait(max(0.1, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(node.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        for node in nodes:
            node.stop()
        if self.phone:
            self.phone.close()


def append_rows(path, rows):
    new = not path.exists()
    with path.open('a', newline='') as f:
        w = csv.DictWriter(f, fieldnames=COLUMNS, extrasaction='ignore')
        if new:
            w.writeheader()
        w.writerows(rows)


def revisions():
    """The workspace's commit; in a container image, PDN_REVISION as baked at build."""
    if os.environ.get('PDN_REVISION'):
        return {'image': os.environ['PDN_REVISION']}
    res = subprocess.run(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'], capture_output=True, text=True)
    dirty = subprocess.run(['git', '-C', str(ROOT), 'status', '--porcelain'], capture_output=True, text=True)
    return {'workspace': res.stdout.strip() + (' +dirty' if dirty.stdout.strip() else '')}


# --- one pod ---------------------------------------------------------------------------------------------------------

class Pod:
    """One pod of N nodes for one N and one S: `writers[0]` owns it, `r` writes nothing."""

    def __init__(self, run, n, s):
        self.run, self.n, self.s = run, n, s
        self.prefix = f'n{n}s{s}'
        self.writers = []
        self.r = None
        self.incarnation = 0
        self.held = 0
        self.seq = collections.Counter()
        self.broken = None
        self.evidence = ''
        self.streams = []  # every stream the pod's nodes opened, a departed r's included

    @property
    def nodes(self):
        return self.writers + [self.r]

    def form(self):
        owner = Node(self.run, f'{self.prefix}-w1').start()
        self.pod = owner.call('POST', '/pods')['pod']
        self.writers.append(owner)
        owner_stream = owner.open_stream(self.pod, track=False)
        self.streams.append(owner_stream)
        for i in range(2, self.n):
            self.writers.append(self.join(Node(self.run, f'{self.prefix}-w{i}').start()))
        self.r = self.join(self.run.reader(f'{self.prefix}-r0').start(), track=True)

    def join(self, node, track=False):
        invite = self.writers[0].call('POST', f'/pods/{self.pod}/invites')
        node.call('POST', '/pods/join', invite, timeout=120)
        self.streams.append(node.open_stream(self.pod, track))
        return node

    def payload(self, writer, tag):
        self.seq[writer.name] += 1
        head = f'{self.run.meta["run"]}|{writer.name}|{self.seq[writer.name]}|{tag}|'.encode()
        if len(head) > self.s:
            raise ValueError(f'payload head {head!r} exceeds S={self.s}')
        return head + b'.' * (self.s - len(head))

    def place(self, writer, conn, tag):
        """The placed record's id."""
        return request(writer.port, 'POST', f'/debug/identities/{writer.identity}/pods/{self.pod}/records?kind={KIND}',
                       self.payload(writer, tag), conn=conn)['id']

    def writing(self, schedule, tag):
        """Each writer places its share of `schedule` (wall times, or None for as fast as it answers) in a thread of
        its own: (placed, failures); the placed records' ids land in `self.ids`."""
        placed, failures = collections.Counter(), []
        self.calls = []
        self.ids = set()

        def write(w, times):
            conn = http.client.HTTPConnection('127.0.0.1', w.port, timeout=STATUS_S)
            consecutive = 0
            for at in times:
                if at is not None:
                    time.sleep(max(0.0, at - time.time()))
                try:
                    c0 = time.monotonic()
                    self.ids.add(self.place(w, conn, tag))
                    self.calls.append((time.monotonic(), time.monotonic() - c0))
                    placed[w.name] += 1
                    consecutive = 0
                except (NodeDown, RuntimeError) as e:
                    conn = http.client.HTTPConnection('127.0.0.1', w.port, timeout=STATUS_S)
                    failures.append(str(e))
                    consecutive += 1
                    if consecutive >= 10:
                        return
            conn.close()

        threads = [threading.Thread(target=write, args=(w, schedule[i::len(self.writers)]), daemon=True)
                   for i, w in enumerate(self.writers)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        total = sum(placed.values())
        self.held += total
        return total, failures

    def fill(self, level):
        """Places records until the pod holds `level`: None once `r` holds every one placed, else why not, and the
        evidence of a stalled stream."""
        need = level - self.held
        rec = {'N': self.n, 'S': self.s, 'level': level, 'held_before': self.held, 'need': max(need, 0)}
        self.run.meta['fills'].append(rec)
        if need <= 0:
            return None, ''
        stream, mark = self.r.stream, len(self.r.stream.order)
        log_from = {self.r.name: self.r.log.stat().st_size}
        t0 = time.monotonic()
        placed, failures = self.writing([None] * need, 'fill')
        rec.update(placed=placed, failures=failures[:5], failures_n=len(failures),
                   place_s=round(time.monotonic() - t0, 1),
                   rate_per_s=round(placed / max(time.monotonic() - t0, 1e-9), 1))
        # One write call's duration as the pod grows: [records held after the bucket, p50 ms, p90 ms] per tenth.
        calls = [d for _, d in sorted(self.calls)]
        step = max(1, len(calls) // 10)
        rec['call_ms'] = [[self.held - placed + min(i + step, len(calls)),
                           round(statistics.median(calls[i:i + step]) * 1e3, 1),
                           round(sorted(calls[i:i + step])[int(0.9 * (len(calls[i:i + step]) - 1))] * 1e3, 1)]
                          for i in range(0, len(calls), step)]
        deadline = t0 + max(FILL_MIN_S, level / FILL_RECORDS_PER_S)
        # No record readable on r for a whole pod interval after the writers finished: a pass delivered nothing.
        stall = GRID_FILL_STALL_S if self.run.meta.get('grid') else self.run.meta['pod_interval_s'] + SILENT_S
        why, held, moved = None, -1, time.monotonic()
        while held < placed:
            if stream.held_since(mark) != held:
                held, moved = stream.held_since(mark), time.monotonic()
                continue
            if time.monotonic() > deadline or stream.ended:
                why = 'fill_timeout'
                break
            if time.monotonic() - moved > stall:
                why = 'fill_stalled'
                break
            time.sleep(0.5)
        rec.update(r_ready_s=round(time.monotonic() - t0, 1), r_ready=stream.held_since(mark), outcome=why)
        evidence = ''
        if why == 'fill_stalled':
            rec['events_silent_s'] = round(time.monotonic() - stream.last, 1) if stream.last else None
            name = f'stalled/{self.run.meta["test"]}-{self.run.meta["variant"]}-N{self.n}-V{level}-S{self.s}-fill'
            evidence = keep_evidence(self.run, name, [self.r], log_from)
            rec['evidence'] = evidence
        listing = bare_listing(self.r, self.pod)
        rec['listing_entries'] = len(listing)
        rec['listing_present'] = sum(1 for e in listing if e['present'])
        rec['listing_counted'] = sum(1 for e in listing if e['verdict'] == 'counted')
        self.run.save()
        return why, evidence

    def check(self):
        for node in self.nodes:
            node.status()

    def predicted(self, t0, t1):
        interval = self.run.meta['pod_interval_s']
        return [(node.name, t) for node in self.nodes for t in predicted_passes(node.started, interval, t0, t1)]

    def stop(self):
        for node in self.nodes:
            if node:
                node.stop()
        for st in self.streams:
            st.close()
            if st.file:
                st.file.close()


# --- a cell ----------------------------------------------------------------------------------------------------------

def quiet(run, pod):
    """Every node under QUIET_PCT of one core for QUIET_S seconds; False when QUIET_BOUND_S ran out first."""
    t0 = time.monotonic()
    time.sleep(QUIET_S + 1)
    while time.monotonic() - t0 < QUIET_BOUND_S:
        t = time.monotonic()
        windows = [run.sampler.window(node.pid, t - QUIET_S, t) for node in pod.nodes]
        if all(w and w[1] < QUIET_PCT for w in windows):
            return True
        time.sleep(1)
    return False


def idle(run, pod, rep, cell):
    long = rep == 0 and not run.meta.get('grid')
    dur = run.meta['pod_interval_s'] + LONG_IDLE_EXTRA_S if long else IDLE_S
    t0, t0_us = time.monotonic(), now_us()
    time.sleep(dur)
    t1, t1_us = time.monotonic(), now_us()
    r = run.sampler.window(pod.r.pid, t0, t0 + IDLE_S)
    whole = run.sampler.window(pod.r.pid, t0, t1)
    cell['idle'] = {'s': dur, 'host_share': run.sampler.host_share(t0, t1),
                    'cpu': {n.name: run.sampler.window(n.pid, t0, t1) for n in pod.nodes}}
    # The pilot's check of the pass prediction: each predicted pass against the sessions its node dialed.
    sessions = [(n.name, st[1], st[0]) for n in pod.nodes for st in n.stream.sessions
                if t0_us <= st[1] <= t1_us and st[5] == 'connect']
    checks = []
    for name, t in pod.predicted(t0, t1):
        at_us = t0_us + (t - t0) * 1e6
        dialed = [(s - at_us) / 1e6 for nm, s, _ in sessions if nm == name]
        checks.append({'node': name, 'predicted_s': round(t - t0, 1),
                       'nearest_connect_s': round(min(dialed, key=abs), 1) if dialed else None})
    cell['idle']['passes'] = checks
    cell['idle']['connect_sessions'] = [(nm, round((s - t0_us) / 1e6, 1), store) for nm, s, store in sessions]
    return {'idle_r_cpu_pct': fmt(r and r[1]), 'idle_long_r_cpu_pct': fmt(whole and whole[1]) if long else ''}, \
        run.sampler.host_share(t0, t1)


def steady(run, pod, cell):
    tag = f'steady-{cell["V"]}-{cell["repeat"]}'
    mark = len(pod.r.stream.order)
    start = time.time() + 1
    t0, t0_us = time.monotonic() + 1, int(start * 1e6)
    total = STEADY_S * STEADY_RATE
    placed, failures = pod.writing([start + k / STEADY_RATE for k in range(total)], tag)
    last = time.monotonic()
    while pod.r.stream.held_since(mark) < placed and time.monotonic() < last + DRAIN_S and not pod.r.stream.ended:
        time.sleep(0.2)
    t1, t1_us = time.monotonic(), now_us()
    held = pod.r.stream.held_since(mark)
    p50, p90, p99 = percentiles(pod.r.stream.latencies_s(mark))
    cpu = run.sampler.window(pod.r.pid, t0, t1)
    listing = bare_listing(pod.r, pod.pod)
    from_steady = sum(1 for e in listing if e['present'] and record_id(e['key']) in pod.ids)
    sessions = sessions_carrying(pod.streams, t0_us, t1_us)
    cell['steady'] = {'placed': placed, 'failures': failures[:5], 'failures_n': len(failures), 'r_held': held,
                      'listing_from_steady': from_steady, 'sessions': sessions, 's': round(t1 - t0, 1),
                      'r_cpu': cpu, 'host_share': run.sampler.host_share(t0, t1),
                      'cpu': {n.name: run.sampler.window(n.pid, t0, t1) for n in pod.nodes},
                      'rss_mb': {n.name: run.sampler.rss_mb(n.pid) for n in pod.nodes}}
    if run.phone:
        process = run.sampler.window(PhoneNode.PID, t0, t1, lambda s: s['cpu_user_us'] + s['cpu_sys_us'])
        cell['steady']['phone_process_cpu'] = process
    values = {'steady_p50_s': fmt(p50), 'steady_p90_s': fmt(p90), 'steady_p99_s': fmt(p99),
              # The phone's process less its node: the application, the agent and threads no one named.
              'observation_cpu_s': fmt(process[0] - cpu[0] if run.phone and process and cpu else None),
              'sessions_per_record': fmt(sessions / placed if placed else None),
              'r_cpu_s_per_record': fmt(cpu[0] / from_steady if cpu and from_steady else None),
              'r_rss_mb': fmt(run.sampler.rss_mb(pod.r.pid))}
    return values, held < placed, (t0, t1), run.sampler.host_share(t0, t1)


def leave(pod):
    """`r` leaves, and the writers' membership no longer lists it: a departed member would be dialed for the rest
    of the run."""
    old = pod.r
    try:
        old.call('POST', f'/pods/{pod.pod}/acts', 'leave')
    except RuntimeError as e:
        # A join whose catch-up timed out leaves the node without the pod; the writers may still list it.
        if 'pod not held' not in str(e):
            raise
    deadline = time.monotonic() + LEAVE_S
    removed = False
    while True:
        listed = [m['id'] for w in pod.writers for m in w.call('GET', f'/pods/{pod.pod}/members')['members']]
        if old.identity not in listed:
            break
        if time.monotonic() > deadline:
            if removed:
                raise RuntimeError(f'{old.name} still a member after its leave and a removal')
            pod.writers[0].call('POST', f'/pods/{pod.pod}/acts', {'remove': old.identity})
            removed, deadline = True, time.monotonic() + LEAVE_S
        time.sleep(0.5)
    old.stop()
    shutil.rmtree(old.data, ignore_errors=True)
    return removed


def catchup(run, pod, cell):
    held = pod.held
    cell['catchup'] = {'held': held, 'removed_after_leave': leave(pod)}
    pod.incarnation += 1
    r = run.reader(f'{pod.prefix}-r{pod.incarnation}').start()
    pod.r = r
    inviter = pod.writers[0]
    invite = inviter.call('POST', f'/pods/{pod.pod}/invites')
    log_from = {n.name: n.log.stat().st_size for n in (r, inviter)}
    t0, t0_us = time.monotonic(), now_us()
    stream = r.open_stream(pod.pod, True, join_invite=invite)
    pod.streams.append(stream)
    deadline = t0 + max(CATCHUP_MIN_S, held / CATCHUP_RECORDS_PER_S)
    listed_us, next_listing = None, t0
    while time.monotonic() < deadline:
        if stream.answered.is_set() and (len(stream.ready) >= held or stream.ended):
            break
        # A lost event leaves the count short for good, so the joiner's own listing says what it holds
        # (the `Lagged` contract of the store's change subscription).
        if stream.gaps and stream.answered.is_set() and time.monotonic() >= next_listing:
            next_listing = time.monotonic() + CATCHUP_LISTING_EVERY_S
            try:
                complete = listed_complete(bare_listing(r, pod.pod), held)
            except (RuntimeError, NodeDown):
                complete = False
            if complete:
                listed_us = now_us()
                break
        time.sleep(0.2)
    t1 = time.monotonic()
    done = stream.status == 200 and (len(stream.ready) >= held or listed_us is not None)
    rec = cell['catchup']
    rec.update(answered=stream.status, error=stream.error, ready=len(stream.ready), lagged=stream.gaps,
               listed=listed_us is not None)
    rec.update(inserted=len(stream.order), events_silent_s=round(t1 - stream.last, 1) if stream.last else None,
               answered_s=round(stream.answered_at - t0, 1) if stream.answered_at else None)
    rec['host_share'] = run.sampler.host_share(t0, t1)
    rec['joiner_cpu'] = run.sampler.window(r.pid, t0, t1)
    rec['serving_cpu'] = {w.name: run.sampler.window(w.pid, t0, t1) for w in pod.writers}
    rec['rss_mb'] = run.sampler.rss_mb(r.pid)
    values, hung = {}, False
    if done:
        last = max(stream.ready.values()) if stream.ready else t0_us
        if listed_us is not None:
            # The listing's own time bounds the end from above; the last readable record seen, from below.
            rec['last_ready_s'] = round((last - t0_us) / 1e6, 3)
            last = listed_us
        secs = (last - t0_us) / 1e6
        values = {'catchup_s': fmt(secs), 'catchup_records_per_s': fmt(held / secs if secs > 0 else None)}
        rec['bytes_per_s'] = held * pod.s / secs if secs > 0 else None
    elif not stream.answered.is_set() or stream.status is None:
        hung = silent(r, pod)
        rec['silent_probe'] = hung
    return values, (not done), hung, (r, inviter, log_from), (t0, t1), rec['host_share']


def silent(r, pod):
    """Whether `r`'s node yields no event of the pod for SILENT_S seconds on a stream of its own."""
    conn = http.client.HTTPConnection('127.0.0.1', r.port, timeout=SILENT_S)
    t0 = time.monotonic()
    try:
        conn.request('GET', f'/debug/identities/{r.identity}/pods/{pod.pod}/events')
        resp = conn.getresponse()
        if resp.status != 200:
            time.sleep(max(0.0, SILENT_S - (time.monotonic() - t0)))
            return True
        line = resp.readline()
        return not line.strip() and time.monotonic() - t0 >= SILENT_S - 1
    except (socket.timeout, TimeoutError):
        return True
    except (OSError, http.client.HTTPException):
        time.sleep(max(0.0, SILENT_S - (time.monotonic() - t0)))
        return True
    finally:
        conn.close()


def keep_evidence(run, name, nodes, log_from):
    """The last events, the logs since `log_from` and a stack sample of the first node, under `name`."""
    out = run.dir / name
    out.mkdir(parents=True, exist_ok=True)
    stack, pid = out / f'stack-{nodes[0].name}.txt', nodes[0].pid
    # The phone's node is not a laptop process: no stack sample of it.
    if isinstance(pid, int) and shutil.which('sample'):
        subprocess.run(['sample', str(pid), '5', '-file', str(stack)], capture_output=True)
    elif isinstance(pid, int) and shutil.which('eu-stack'):
        stack.write_text(subprocess.run(['eu-stack', '-p', str(nodes[0].pid)], capture_output=True,
                                        text=True).stdout)
    for node in nodes:
        tail = list(node.stream.tail) if node.stream else []
        (out / f'events-{node.name}.jsonl').write_text(''.join(line if line.endswith('\n') else line + '\n'
                                                               for line in tail))
        with node.log.open('rb') as f:
            f.seek(log_from[node.name])
            (out / f'log-{node.name}.txt').write_bytes(f.read())
    return name


def listed_complete(listing, held):
    """Whether a listing holds `held` records with their payloads."""
    return sum(1 for e in listing if e['present']) >= held


def bare_listing(node, pod):
    """`node`'s record listing without payloads, which a phone holding every payload of a large pod at once would be
    killed for; a host older than the query answers 400 and is asked for the listing with them."""
    try:
        return node.call('GET', f'/pods/{pod}/listing?payloads=false', timeout=600)
    except RuntimeError as e:
        if ': 400 ' not in str(e):
            raise
        return node.call('GET', f'/pods/{pod}/listing', timeout=600)


def record_id(key):
    """The record id of a record-store key, `by/<member>/<kind>/<id>/<n>`."""
    parts = key.split('/')
    return parts[3] if len(parts) > 3 else None


def fmt(x):
    return '' if x is None else f'{x:.6g}'


def run_cell(run, pod, v, rep):
    """One cell: quiet wait, idle, steady, catch-up; the row it appends."""
    cell = {'N': pod.n, 'V': v, 'S': pod.s, 'repeat': rep, 'V_actual': pod.held, 'started': time.time(),
            'free_disk_gb': round(shutil.disk_usage(run.dir).free / 1e9, 1)}
    run.meta['cells'].append(cell)
    t_cell = time.monotonic()
    lagged0 = sum(st.gaps for st in pod.streams)
    v_actual = pod.held
    try:
        pod.check()
    except NodeDown as e:
        cell['error'] = str(e)
        run.save()
        raise
    marks = {}
    if run.phone:
        deadline = time.monotonic() + THERMAL_WAIT_S
        while run.sampler.thermal() not in ('nominal', None) and time.monotonic() < deadline:
            time.sleep(5)
        marks['thermal'] = '0' if run.sampler.thermal() in ('nominal', None) else '1'
        t_cell = time.monotonic()
    marks['not_quiet'] = '0' if quiet(run, pod) else '1'
    cell['quiet_s'] = round(time.monotonic() - t_cell, 1)
    values, idle_share = idle(run, pod, rep, cell)
    steady_values, steady_short, steady_win, steady_share = steady(run, pod, cell)
    values.update(steady_values)
    cu_values, cu_short, hung, (joiner, inviter, log_from), cu_win, cu_share = catchup(run, pod, cell)
    values.update(cu_values)
    lagged = sum(st.gaps for st in pod.streams) - lagged0
    passes = pod.predicted(*steady_win) + pod.predicted(*cu_win)
    cell['pod_passes_in_measured'] = [(nm, round(t - t_cell, 1)) for nm, t in passes]
    slept = any(t_cell <= j for j in run.sampler.jumps)
    marks.update(contended='1' if max(idle_share, steady_share, cu_share) > CONTENDED_SHARE else '0',
                 lagged=str(lagged), incomplete='1' if (steady_short or cu_short) and not hung else '0',
                 join_hung='1' if hung else '0', slept='1' if slept else '0', pod_pass='1' if passes else '0')
    if run.phone:
        marks.update(run.sampler.phone_marks(t_cell, time.monotonic()))
    if hung:
        marks['evidence'] = keep_evidence(
            run, f'hung/{run.meta["test"]}-{run.meta["variant"]}-N{pod.n}-V{v}-S{pod.s}-r{rep}', [joiner, inviter],
            log_from)
        pod.broken, pod.evidence = 'join_hung', marks['evidence']
        joiner.stop()
    cell['s'] = round(time.monotonic() - t_cell, 1)
    run.save()
    return run.row(pod.n, v, pod.s, rep, v_actual, **marks, **values)


def obs_cost(run, pod):
    """`r`'s CPU over one steady workload with its event stream open, then over one with it closed."""
    out = {'N': pod.n, 'S': pod.s, 'held': pod.held}
    for label in ('open', 'closed'):
        if label == 'closed':
            pod.r.stream.close()
            time.sleep(2)
        start = time.time() + 1
        t0, seen = time.monotonic() + 1, pod.r.stream.count
        pod.writing([start + k / STEADY_RATE for k in range(STEADY_S * STEADY_RATE)], f'obs-{label}')
        time.sleep(30)
        out[label] = run.sampler.window(pod.r.pid, t0, time.monotonic())
        out[f'{label}_events'] = pod.r.stream.count - seen
    pod.streams.append(pod.r.open_stream(pod.pod, True))
    run.meta['obs_cost_runs'].append(out)
    run.save()
    cpu = {k: fmt(out[k] and out[k][0]) for k in ('open', 'closed')}
    print(f'  observation cost N={pod.n} held={out["held"]}: r CPU s open {cpu["open"]}, closed {cpu["closed"]}',
          flush=True)


def run_n(run, n):
    for s in run.meta['S']:
        pod = Pod(run, n, s)
        levels = run.levels(s)
        try:
            pod.form()
            for v in levels:
                why, evidence = pod.fill(v) if pod.broken is None else (None, '')
                if why == 'fill_stalled':
                    pod.broken, pod.evidence = why, evidence
                for rep in range(run.meta['repeats']):
                    if pod.broken or why:
                        run.row(n, v, s, rep, pod.held, not_run=pod.broken or why, evidence=pod.evidence)
                        continue
                    row = run_cell(run, pod, v, rep)
                    print(summary_line(row, run.meta['cells'][-1]), flush=True)
                if run.meta['obs_cost'] and not pod.broken and not why:
                    obs_cost(run, pod)
        finally:
            pod.stop()
            for node in pod.writers + ([pod.r] if pod.r else []):
                shutil.rmtree(node.data, ignore_errors=True)
    for path in (run.dir / 'events').glob(f'n{n}s*.jsonl'):
        with path.open('rb') as src, gzip.open(f'{path}.gz', 'wb') as dst:
            shutil.copyfileobj(src, dst)
        path.unlink()


def summary_line(row, cell):
    marks = [m for m in ('contended', 'incomplete', 'join_hung', 'slept', 'not_quiet', 'pod_pass') if row.get(m) == '1']
    marks += [f'lagged {row["lagged"]}'] if row.get('lagged') not in ('', '0') else []
    st = cell.get('steady', {})
    cu = cell.get('catchup', {})
    return (f'N={row["N"]} V={row["V"]} (held {row["V_actual"]}) S={row["S"]} rep {row["repeat"]}: cell {cell["s"]} s; '
            f'p50 {row["steady_p50_s"]} p99 {row["steady_p99_s"]} s; catch-up {row["catchup_s"] or "-"} s '
            f'({row["catchup_records_per_s"] or "-"}/s); r CPU s/rec {row["r_cpu_s_per_record"]}; '
            f'sessions/rec {row["sessions_per_record"]}; idle r % {row["idle_r_cpu_pct"]}; '
            f'steady placed {st.get("placed")} held {st.get("r_held")}; catch-up ready {cu.get("ready")}/{cu.get("held")}'
            + (f'; marks: {", ".join(marks)}' if marks else ''))


# --- the ramp: one pod per N, filled at a constant rate, measured as it grows ----------------------------------------

RAMP_STEP, RAMP_STALL_S, RAMP_BEHIND_MS = 500, 60, 30_000
# One printed line per step: write → readable on r (median, p95), r's CPU per record, the laptop's load.
RAMP_ROW = '| {:<8} | {:>5} | {:>7} | {:>9} | {:>7} | {:>10} | {:>12} | {:>8} | {:<15} |'
RAMP_COLUMNS = ['run', 'N', 'held', 'records', 'p50_ms', 'p95_ms', 'r_cpu_ms_per_record', 'r_cpu_pct', 'host_share',
                'write_call_p50_ms', 'sessions_per_record', 'stalled']


def ramp(run, n, rate, to):
    """Writers place `rate` records a second until the pod holds `to`; one row per RAMP_STEP records. A step whose
    content `r` has not all received after RAMP_STALL_S without progress ends the ramp, marked stalled."""
    pod = Pod(run, n, 256)
    out = run.dir / 'ramp.csv'
    new = not out.exists()
    try:
        pod.form()
        with out.open('a', newline='') as f:
            w = csv.DictWriter(f, RAMP_COLUMNS)
            if new:
                w.writeheader()
            print(RAMP_ROW.format('time', 'nodes', 'records', 'median ms', 'p95 ms', 'CPU ms/rec',
                                  'sessions/rec', 'laptop %', 'content'), flush=True)
            print(RAMP_ROW.format(*[''] * 9).replace(' ', '-'), flush=True)
            while pod.held < to:
                mark = len(pod.r.stream.order)
                count = min(100 if pod.held < 500 else RAMP_STEP, to - pod.held)
                start = time.time() + 0.2
                t0, t0_us = time.monotonic() + 0.2, int(start * 1e6)
                placed, _ = pod.writing([start + k / rate for k in range(count)], f'ramp-{pod.held}')
                calls = [d for _, d in pod.calls]
                held, moved = -1, time.monotonic()
                while pod.r.stream.held_since(mark) < placed and time.monotonic() - moved < RAMP_STALL_S:
                    if pod.r.stream.held_since(mark) != held:
                        held, moved = pod.r.stream.held_since(mark), time.monotonic()
                    time.sleep(0.2)
                t1, t1_us = time.monotonic(), now_us()
                got = pod.r.stream.held_since(mark)
                lat = sorted(pod.r.stream.latencies_s(mark))
                cpu = run.sampler.window(pod.r.pid, t0, t1)
                stalled = got < placed
                behind = bool(lat) and lat[len(lat) // 2] * 1e3 > RAMP_BEHIND_MS
                row = {'run': run.meta['run'], 'N': n, 'held': pod.held, 'records': len(lat),
                       'p50_ms': fmt(lat[len(lat) // 2] * 1e3 if lat else None),
                       'p95_ms': fmt(lat[min(len(lat) - 1, int(0.95 * len(lat)))] * 1e3 if lat else None),
                       'r_cpu_ms_per_record': fmt(cpu[0] / got * 1e3 if cpu and got else None),
                       'r_cpu_pct': fmt(cpu[1] if cpu else None), 'host_share': fmt(run.sampler.host_share(t0, t1)),
                       'write_call_p50_ms': fmt(statistics.median(calls) * 1e3 if calls else None),
                       'sessions_per_record': fmt(sessions_carrying(pod.streams, t0_us, t1_us) / placed if placed else None),
                       'stalled': int(stalled)}
                w.writerow(row)
                f.flush()
                ms = lambda k: f'{float(row[k]):,.0f}' if row[k] else '-'
                print(RAMP_ROW.format(time.strftime('%H:%M:%S'), n, f'{pod.held:,}', ms('p50_ms'), ms('p95_ms'),
                                      ms('r_cpu_ms_per_record'),
                                      f'{float(row["sessions_per_record"]):.1f}' if row['sessions_per_record'] else '-',
                                      f'{float(row["host_share"] or 0) * 100:.0f}',
                                      'CONTENT STOPPED' if stalled else 'BEHIND > 30 s' if behind else 'ok'), flush=True)
                if stalled:
                    name = keep_evidence(run, f'stalled/ramp-N{n}-at{pod.held}', [pod.r], {pod.r.name: 0})
                    # What `r` holds against what its events reported: a silent stream and a stalled sync differ here.
                    listing = bare_listing(pod.r, pod.pod)
                    held_now = {'events_held': pod.held - placed + got, 'listing_entries': len(listing),
                                'listing_present': sum(1 for e in listing if e['present']),
                                'events_silent_s': round(time.monotonic() - pod.r.stream.last, 1)
                                if pod.r.stream.last else None}
                    (run.dir / name / 'listing.json').write_text(json.dumps(held_now) + '\n')
                    print(f'  r: {held_now}', flush=True)
                    return
                if behind:
                    return
            if run.meta.get('skip_catchup'):
                return
            cell = {}
            values, incomplete, hung, _, _, _ = catchup(run, pod, cell)
            print(f'N={n} catch-up of {cell["catchup"]["held"]}: {values or "incomplete"}{" hung" if hung else ""}',
                  flush=True)
            run.meta.setdefault('ramp_catchup', []).append({'N': n, **values, 'incomplete': incomplete, 'hung': hung,
                                                            'ready': cell['catchup'].get('ready'),
                                                            'lagged': cell['catchup'].get('lagged'),
                                                            'listed': cell['catchup'].get('listed'),
                                                            'last_ready_s': cell['catchup'].get('last_ready_s')})
    finally:
        pod.stop()
        for node in pod.writers + ([pod.r] if pod.r else []):
            shutil.rmtree(node.data, ignore_errors=True)
        run.save()


def relaunch(run):
    """A fresh process of the phone's test build after a failure, so the retry does not inherit a stuck one."""
    try:
        run.launch()
    except Exception as e:
        print(f'the phone did not come back: {e!r}', flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ints = lambda text: [int(x) for x in text.split(',')]
    ap.add_argument('--n', type=ints, default=[2, 4, 8, 16])
    ap.add_argument('--v', type=ints, default=[0, 1_000, 10_000, 100_000])
    ap.add_argument('--s', type=ints, default=[256, 65_536])
    ap.add_argument('--repeats', type=int, default=3)
    ap.add_argument('--run-id', help='resumes the run of that id when its directory exists')
    ap.add_argument('--pod-interval', type=int, help=f'seconds; default the runtime\'s {DEFAULT_POD_INTERVAL_S}')
    ap.add_argument('--variant', choices=['plain', 'phone'], default='plain',
                    help='phone: test P, the phone on the cable as r (pdn-app/sync-test/phone.py)')
    ap.add_argument('--bin', default=str(DEFAULT_BIN))
    ap.add_argument('--out', help='where run directories go; default tmp/load in the workspace')
    ap.add_argument('--nodes-dir', help="where the nodes' stores go; default nodes/ in the run directory")
    ap.add_argument('--obs-cost', action='store_true', help='after each pod\'s last level, r\'s CPU with and '
                    'without its event stream (a pilot gate)')
    ap.add_argument('--grid', action='store_true', help='short windows (idle 10 s, steady 20 s, catch-up from 45 s, '
                    'fill stall 60 s) for an N × V map; run once per V for a fresh pod per cell')
    ap.add_argument('--ramp', type=int, metavar='RATE', help='records a second: fill one pod per N at this '
                    'rate up to the largest --v, measured every 500 records (ramp.csv)')
    ap.add_argument('--skip-catchup', action='store_true', help='--ramp: end without the wipe-and-rejoin')
    ap.add_argument('--selfcheck', action='store_true')
    args = ap.parse_args()
    if args.grid:
        global IDLE_S, STEADY_S, DRAIN_S, CATCHUP_MIN_S
        IDLE_S, STEADY_S, DRAIN_S, CATCHUP_MIN_S = 10, 20, 20, 45
    if args.selfcheck:
        return selfcheck()
    in_workspace = (ROOT / 'Cargo.toml').exists() and (ROOT / 'crates' / 'pdn-node-http').exists()
    if not in_workspace and (args.bin == str(DEFAULT_BIN) or not args.out):
        sys.exit(f'{ROOT} is not the PDN workspace: name the node binary with --bin and the results with --out')
    args.out = args.out or str(ROOT / 'tmp' / 'load')

    # A cancelled CI job or a closed terminal ends the run through the finally below, which stops every node.
    def stop(sig, _frame):
        raise SystemExit(128 + sig)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, stop)
    args.run_id = args.run_id or time.strftime('L-%Y%m%d-%H%M%S')
    run = Run(args)
    run.meta['grid'] = args.grid
    run.meta['skip_catchup'] = args.skip_catchup
    run.meta['ramp'] = args.ramp
    if args.ramp:
        print(f'run {run.meta["run"]} in {run.dir}', flush=True)
        try:
            for n in args.n:
                ramp(run, n, args.ramp, max(args.v))
        finally:
            run.stop_all()
            run.meta.update(finished=time.time(), host_after=host_info())
            run.save()
        return 0
    print(f'run {run.meta["run"]} in {run.dir}; N order {run.meta["N"]}', flush=True)
    try:
        for n in run.meta['N']:
            if n in run.done:
                continue
            for attempt in (1, 2):
                try:
                    run_n(run, n)
                    break
                except (NodeDown, RuntimeError) as e:
                    print(f'N={n}: {e}', flush=True)
                    run.meta.setdefault('restarts', []).append({'N': n, 'error': str(e)})
                    run.discard(n)
                    if run.phone:
                        relaunch(run)
                    if attempt == 2:
                        for s in run.meta['S']:
                            for v in run.levels(s):
                                for rep in range(run.meta['repeats']):
                                    run.row(n, v, s, rep, '', not_run='node_down')
    finally:
        run.stop_all()
        run.meta.update(finished=time.time(), host_after=host_info())
        run.meta['sleep_jumps'] = len(run.sampler.jumps)
        run.save()
    return 0


if __name__ == '__main__':
    sys.exit(main())
