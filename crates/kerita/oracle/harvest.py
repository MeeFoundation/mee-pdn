"""Runs keripy's own tests at the pin and records what they build.

A keripy test passes only when what keripy builds equals the literal values
the test asserts, so every primitive, indexed signature, counter and event
recorded during a passing test is one of that test's cases, values included.
A record made during a failing test is dropped, and the test is named.
"""
import dataclasses
import functools

import pytest

from keri.core import coring, counting, eventing, indexing, serdering

import determinism


class Harvest:
    """A pytest plugin: what each passing test built, keyed by its node id."""

    def __init__(self):
        self.current = None
        self.pending = []
        self.keverys = {}
        self.records = {}
        self.failed = []

    def label(self, kevery):
        """The validators of one test, numbered in the order they first act."""
        return self.keverys.setdefault(id(kevery), len(self.keverys))

    def record(self, kind, entry):
        if self.current is not None:
            self.pending.append((kind, entry))

    @pytest.hookimpl(hookwrapper=True)
    def pytest_runtest_call(self, item):
        self.current = item.nodeid
        self.pending = []
        self.keverys = {}
        outcome = yield
        if outcome.excinfo is None:
            self.records[item.nodeid] = self.pending
        self.current = None
        self.pending = []

    def pytest_runtest_logreport(self, report):
        if report.failed and report.nodeid not in self.failed:
            self.failed.append(report.nodeid)


def _wrap_init(cls, harvest, kind, describe, parsed=("qb64", "qb64b")):
    original = cls.__init__

    @functools.wraps(original)
    def init(self, *args, **kwargs):
        try:
            original(self, *args, **kwargs)
        except Exception as error:
            source = _parsed_from(kwargs, parsed)
            if source is not None:
                harvest.record(kind, {"refused": source, "error": type(error).__name__})
            raise
        # Built with its checks off, as a test builds an event for the key state to refuse:
        # recorded as keripy's parser answers it, checks on.
        if kwargs.get("verify") is False and hasattr(self, "_verify"):
            try:
                self._verify()
            except Exception as error:
                source = _text(self.raw)
                if source is not None:
                    harvest.record(kind, {"refused": source, "error": type(error).__name__})
                return
        # A record that cannot be made is dropped: the harvest never fails a test it watches.
        try:
            entry = describe(self)
        except Exception:
            return
        harvest.record(kind, entry)

    cls.__init__ = init


def _parsed_from(kwargs, keys):
    """The text a refused parse was given, when it was given text."""
    for key in keys:
        value = kwargs.get(key)
        if value:
            return _text(value) if isinstance(value, (bytes, bytearray)) else value
    return None


def _matter(m):
    return {"class": type(m).__name__, "code": m.code, "raw": m.raw.hex(), "qb64": m.qb64}


def _indexer(i):
    return {"code": i.code, "index": i.index, "ondex": i.ondex, "raw": i.raw.hex(),
            "qb64": i.qb64}


def _counter(c):
    return {"code": c.code, "count": c.count, "qb64": c.qb64,
            "version": f"{c.version.major}.{c.version.minor}"}


def _serder(s):
    return {"raw": _text(s.raw), "sad": s.sad, "kind": s.kind, "said": s.said, "ilk": s.ilk,
            "pvrsn": f"{s.pvrsn.major}.{s.pvrsn.minor}",
            "gvrsn": f"{s.gvrsn.major}.{s.gvrsn.minor}" if s.gvrsn else None}


def _text(raw):
    """The text form of an event, or None for a binary body (outside the subset)."""
    try:
        return bytes(raw).decode("ascii")
    except UnicodeDecodeError:
        return None


def _state(kevery, pre):
    """The key state a validator holds for pre, without its local fields."""
    kever = kevery.kevers.get(pre)
    if kever is None:
        return None
    state = dataclasses.asdict(kever.state())
    return {k: v for k, v in state.items() if k not in ("vn", "f", "dt")}


def _message(serder, sigers, wigers, delsner, delsger):
    """The whole message keripy writes for an event and its attachments, or None."""
    from keri.core.structing import SealSource
    try:
        bonds = ([SealSource(s=delsner, d=delsger)]
                 if delsner is not None and delsger is not None else None)
        return _text(eventing.messagize(serder, sigers=sigers or None, wigers=wigers or None,
                                        bonds=bonds))
    except Exception:
        return None


def _validator(kevery, serder):
    """Who checks the event, as the crate's `Validator` names it: the controller of its AID, one
    of its witnesses, or any other validator. keripy accepts the event without receipts in the
    first two cases, a witness to give its own."""
    own = set(kevery.prefixes)
    kever = kevery.kevers.get(serder.pre)
    witnesses = set(serder.sad.get("b") or []) | set(serder.sad.get("ba") or [])
    witnesses |= set(kever.wits) if kever is not None else set()
    if serder.pre in own:
        return "controller"
    return "witness" if own & witnesses else "remote"


def _wrap_kevery(harvest):
    """Every event a validator is given, with its attachments and the validator's answer.

    keripy's escrows give an escrowed event to processEvent again, so the steps
    of one validator are every attempt in order, the retries included.
    """
    original = eventing.Kevery.processEvent

    @functools.wraps(original)
    def process(self, serder, sigers, **kwargs):
        delsner, delsger = kwargs.get("delsner"), kwargs.get("delsger")
        step = {
            "kevery": harvest.label(self),
            "event": _text(serder.raw),
            "sad": serder.sad,
            "sigs": [s.qb64 for s in sigers or []],
            "wigs": [w.qb64 for w in kwargs.get("wigers") or []],
            "delegation": ({"s": delsner.numh, "d": delsger.qb64}
                           if delsner is not None and delsger is not None else None),
        }
        step["message"] = _message(serder, sigers, kwargs.get("wigers"), delsner, delsger)
        step["validator"] = _validator(self, serder)
        before = _state(self, serder.pre)
        try:
            result = original(self, serder, sigers, **kwargs)
        except Exception as error:
            step["outcome"] = type(error).__name__
            step["state"] = _state(self, serder.pre)
            harvest.record("kel", step)
            raise
        after = _state(self, serder.pre)
        step["outcome"] = "accepted" if after != before else "unchanged"
        step["state"] = after
        harvest.record("kel", step)
        return result

    eventing.Kevery.processEvent = process


def install(harvest):
    determinism.install()
    _wrap_init(coring.Matter, harvest, "matter", _matter)
    _wrap_init(indexing.Indexer, harvest, "indexer", _indexer)
    _wrap_init(counting.Counter, harvest, "counter", _counter)
    _wrap_init(serdering.Serder, harvest, "event", _serder, parsed=("raw",))
    _wrap_kevery(harvest)


def run(paths, rootdir="/opt/keripy"):
    """Runs the keripy test files named, returns {node id: [(kind, entry)]} and the node ids of
    the tests that failed."""
    harvest = Harvest()
    install(harvest)
    code = pytest.main(["-q", "-p", "no:cacheprovider", "--rootdir", rootdir,
                        *[f"{rootdir}/{p}" for p in paths]], plugins=[harvest])
    if code not in (pytest.ExitCode.OK, pytest.ExitCode.TESTS_FAILED):
        raise SystemExit(f"keripy's tests did not run: {code!r}")
    return harvest.records, harvest.failed
