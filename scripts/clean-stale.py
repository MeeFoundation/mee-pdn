#!/usr/bin/env -S python3 -I
"""Removes what no cargo command has read in target/ since the previous run, then marks what is left.

Cargo reads the fingerprint of every unit it considers, fresh ones too; the mark is an access time
older than any modification time, which the next read replaces even under relatime. A first run
only marks, and a profile directory nothing has read since the mark is left whole, so a mount
without access times removes nothing. incremental/ names carry no unit hash: each kept unit keeps
the one crate directory whose newest session began closest to its invoked.timestamp, within a
minute. Each profile directory is cleaned under cargo's own lock on it.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import argparse
import fcntl
import os
import re
import shutil
import time
from collections import Counter, defaultdict
from pathlib import Path

MARK = 946684800  # 2000-01-01, older than every artifact
UNIT = re.compile(r"^(.+?)-([0-9a-f]{16})(\.|$)")
# The session starts after parsing and expansion; the gap measured on this workspace stays under 6 s.
SESSION_AFTER_INVOKED = (-10, 60)


def ls(d):
    try:
        return list(d.iterdir())
    except (FileNotFoundError, NotADirectoryError):
        return []


def files(p):
    if p.is_dir() and not p.is_symlink():
        for dp, _, fs in os.walk(p):
            for f in fs:
                yield Path(dp) / f
    else:
        yield p


def session_start(s):
    try:
        return int(s.name.split("-")[1], 36) / 1e6
    except (IndexError, ValueError):
        return None


def freed_gib(victims):
    # A hard link kept elsewhere (an uplifted copy) frees nothing.
    links, nlink, size = Counter(), {}, {}
    for v in victims:
        for f in files(v):
            try:
                st = f.lstat()
            except FileNotFoundError:
                continue
            k = (st.st_dev, st.st_ino)
            links[k] += 1
            nlink[k], size[k] = st.st_nlink, st.st_blocks * 512
    return sum(size[k] for k in links if links[k] >= nlink[k]) / (1 << 30)


def clean(prof, shown, since, dry):
    # 1. Units by hash, and the ones cargo has read since the mark.
    units = {
        m.group(2): d for d in ls(prof / ".fingerprint") if (m := UNIT.match(d.name))
    }
    read = {h for h, d in units.items() if any(f.stat().st_atime > MARK for f in ls(d))}

    # 2. Nothing read: nobody built here since the mark, or the mount records no access times.
    if not read:
        print(f"{shown}: nothing read since the previous run, left whole")
        return

    # 3. Entries of stale units and orphans (a hash no unit claims); dep-info names each hash's crate.
    stale = set(units) - read
    victims = [units[h] for h in stale]
    crate = {}
    orphans = 0
    for e in [*ls(prof / "deps"), *ls(prof / "examples"), *ls(prof / "build")]:
        m = UNIT.match(e.name)
        if not m:
            continue
        if e.name.endswith(".d"):
            crate[m.group(2)] = m.group(1)
        if m.group(2) in stale or m.group(2) not in units:
            victims.append(e)
            orphans += m.group(2) not in units
    # A build script's dep-info sits inside its own build/ directory.
    for e in (prof / "build").glob("*/*.d"):
        if m := UNIT.match(e.name):
            crate[m.group(2)] = m.group(1)

    # 4. When each kept unit's compilation started, by crate.
    invoked = defaultdict(list)
    for h in read:
        stamp = units[h] / "invoked.timestamp"
        if h in crate and stamp.exists():
            invoked[crate[h]].append((stamp.stat().st_mtime, h))

    # 5. incremental/: kept if rustc wrote it since the mark, else paired with a kept unit by time.
    lo, hi = SESSION_AFTER_INVOKED
    kept, pairs = set(), []
    dirs = ls(prof / "incremental")
    for d in dirs:
        starts = (
            t
            for s in ls(d)
            if s.name.startswith("s-") and (t := session_start(s)) is not None
        )
        newest = max(starts, default=0)
        if newest >= since:
            kept.add(d)
        pairs += [
            (abs(newest - t), h, d)
            for t, h in invoked[d.name.rsplit("-", 1)[0]]
            if lo <= newest - t <= hi
        ]
    # One directory per unit, the closest: configurations built seconds apart would otherwise keep each other's.
    claimed = set()
    for _, h, d in sorted(pairs, key=lambda p: p[0]):
        if h not in claimed and d not in kept:
            claimed.add(h)
            kept.add(d)
    incremental = [d for d in dirs if d not in kept]
    victims += incremental

    # 6. Report; a dry run stops here.
    verb = "would remove" if dry else "removed"
    print(
        f"{shown}: {verb} {len(stale)} of {len(units)} units, {orphans} orphans, "
        f"{len(incremental)} incremental dirs, {freed_gib(victims):.1f} GiB"
    )
    if dry:
        for name, n in Counter(units[h].name[:-17] for h in stale).most_common(10):
            print(f"  {name}: {n} units")
        return

    # 7. Remove.
    for v in victims:
        if v.is_dir() and not v.is_symlink():
            shutil.rmtree(v, ignore_errors=True)
        else:
            v.unlink(missing_ok=True)


def mark(prof):
    for d in ls(prof / ".fingerprint"):
        for f in ls(d):
            os.utime(f, ns=(MARK * 10**9, f.stat().st_mtime_ns))


def main():
    # 1. Flags; an unknown one is an error, so a typo never reaches removal.
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--dry-run",
        action="store_true",
        help="report what would go; remove and mark nothing",
    )
    mode.add_argument(
        "--init",
        action="store_true",
        help="mark when no previous run has; otherwise do nothing",
    )
    args = parser.parse_args()

    # 2. target/ and the time of the previous run's mark.
    target = Path(
        os.environ.get("CARGO_TARGET_DIR")
        or Path(__file__).resolve().parent.parent / "target"
    )
    mark_file = target / ".clean-stale-mark"
    since = float(mark_file.read_text()) if mark_file.exists() else None

    # 3. --init acts only on a target/ no run has marked.
    if args.init and since is not None:
        return

    # 4. The new mark takes this run's start: a session begun while it runs counts as after the mark.
    started = time.time()

    # 5. Profile directories: target/<profile> and target/<triple>/<profile>.
    profiles = sorted(
        p.parent
        for p in [*target.glob("*/.fingerprint"), *target.glob("*/*/.fingerprint")]
    )
    if not profiles:
        print(f"{os.path.relpath(target)}: nothing to clean")
        return

    # 6. Each one under cargo's own lock: a first run only marks, a later one cleans, then marks.
    for prof in profiles:
        shown = os.path.relpath(prof)
        with open(prof / ".cargo-lock", "a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                print(f"{shown}: waiting for cargo to release the directory")
                fcntl.flock(lock, fcntl.LOCK_EX)
            if since is None:
                print(
                    f"{shown}: no previous run to compare against, nothing removed"
                    + (
                        ""
                        if args.dry_run
                        else "; marked, removal starts with the next run"
                    )
                )
            else:
                clean(prof, shown, since, args.dry_run)
            if not args.dry_run:
                mark(prof)

    # 7. Record the mark for the next run.
    if not args.dry_run:
        mark_file.write_text(f"{started}\n")


if __name__ == "__main__":
    main()
