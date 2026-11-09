"""keripy's own tests at the pin, harvested into the corpus: primitives, indexed
signatures, counters, events and the KELs keripy's validators were given.

The tests run twice in separate processes and only what both runs recorded is
kept: a test that draws a random key yields different values each run, and a
corpus holding them would differ from every regeneration.
"""
import hashlib
import json
import os
import subprocess
import sys

from keri.core import coring

import corpus
import gen_replayed

FILES = [
    "tests/core/test_coring.py",
    "tests/core/test_indexing.py",
    "tests/core/test_signing.py",
    "tests/core/test_counting.py",
    "tests/core/test_serdering.py",
    "tests/core/test_eventing_v2.py",
    "tests/core/test_escrow_v2.py",
    "tests/core/test_partial_rotation_v2.py",
]
ILKS = {"icp", "rot", "ixn", "dip", "drt", "rct"}


def records(files, rootdir="/opt/keripy"):
    """{node id: [(kind, entry)]}, as both runs of the tests recorded them, and the tests that
    failed in either; pytest's report goes to stderr."""
    runs = []
    for n in (1, 2):
        out = f"/tmp/harvest-{n}.json"
        subprocess.run([sys.executable, "-I", os.path.join(os.path.dirname(__file__), "oracle.py"),
                        "harvest", out, rootdir, *files], check=True, stdout=sys.stderr)
        with open(out, encoding="ascii") as f:
            runs.append(json.load(f))
    failed = sorted({node for run in runs for node in run["failed"]})
    first, second = (run["records"] for run in runs)
    kept = {}
    for node, entries in first.items():
        again = second.get(node)
        if again is None:
            continue
        if [e for k, e in entries if k == "kel"] != [e for k, e in again if k == "kel"]:
            # A test's validator steps are kept whole or not at all.
            entries = [(k, e) for k, e in entries if k != "kel"]
        seen = {json.dumps(x, sort_keys=True) for x in again}
        kept[node] = [(k, e) for k, e in entries if json.dumps([k, e], sort_keys=True) in seen]
    return kept, failed


def _where(node):
    """The file and test a node id names: keripy's, or a generator of the oracle's."""
    file, _, test = node.partition("::")
    if "tests/" in file:
        return file[file.index("tests/"):], test
    return f"crates/kerita/oracle/{os.path.basename(file)}", test


def _how(node):
    return "copied" if "tests/" in node.partition("::")[0] else "generated"


def _code(qb64):
    try:
        return coring.Matter(qb64=qb64).code
    except Exception:
        return None


def _primitive_vectors(recorded, kind, describe, subset):
    vectors, seen = [], set()
    for node in sorted(recorded):
        for k, entry in recorded[node]:
            if k != kind:
                continue
            text = entry.get("qb64", entry.get("refused"))
            if text is None or text in seen:
                continue
            seen.add(text)
            if "refused" in entry:
                outcome, expect = {"refused": entry["error"]}, "refused"
            else:
                outcome = describe(entry)
                expect = "same" if subset(entry) else "unsupported"
            vectors.append({"kind": kind, "input": {"qb64": text}, "keripy": outcome,
                            "expect": expect, "source": corpus.source(_how(node), *_where(node))})
    return _numbered(kind, vectors)


def _numbered(family, vectors):
    for n, v in enumerate(vectors):
        v["id"] = f"{family}/{n}"
    return vectors


def _sad_codes(sad):
    """Every primitive code an event's fields carry."""
    values = [sad.get(f) for f in ("i", "d", "p", "di")]
    values += [*sad.get("k", []), *sad.get("n", []), *sad.get("b", []),
               *sad.get("br", []), *sad.get("ba", [])]
    for seal in sad.get("a", []) if isinstance(sad.get("a"), list) else []:
        values += [v for k, v in seal.items() if k in ("i", "d")] if isinstance(seal, dict) else []
    return {_code(v) for v in values if isinstance(v, str) and v}


def _readable(event):
    """Whether the crate reads the event: KERI 2.00 native, a subset ilk, subset codes, and
    thresholds without nested weight maps."""
    sad = event["sad"]
    return (event["kind"] == "CESR" and event["pvrsn"] == "2.0" and event["gvrsn"] == "2.0"
            and event["ilk"] in ILKS and _sad_codes(sad) <= corpus.MATTER
            and all(_threshold_ok(sad[f]) for f in ("kt", "nt") if f in sad)
            and _seals_ok(sad))


def _seals_ok(sad):
    """The seals of the subset: a digest seal or a key event seal."""
    seals = sad.get("a", [])
    return isinstance(seals, list) and all(
        isinstance(s, dict) and set(s) in ({"d"}, {"i", "s", "d"}) for s in seals)


def _event_vectors(recorded):
    vectors, seen = [], set()
    for node in sorted(recorded):
        for k, entry in recorded[node]:
            raw = entry.get("raw", entry.get("refused"))
            if k != "event" or raw is None or raw in seen:
                continue
            seen.add(raw)
            if "refused" in entry:
                vectors.append({"kind": "event", "input": {"raw": raw},
                                "keripy": {"refused": entry["error"]}, "expect": "refused",
                                "source": corpus.source(_how(node), *_where(node))})
                continue
            vectors.append({
                "kind": "event", "input": {"raw": entry["raw"]},
                "keripy": {f: entry[f] for f in ("said", "ilk", "kind", "pvrsn", "gvrsn", "sad")},
                "expect": _event_expectation(entry),
                "source": corpus.source(_how(node), *_where(node)),
            })
    return _numbered("event", vectors)


def _event_expectation(entry):
    """same, unsupported, or refused for a body keripy built that is no event: one whose
    SAID or prefix is empty, as keripy's tests build on the way to an event."""
    if entry["ilk"] in ILKS and entry["kind"] == "CESR" and not all(
            entry["sad"].get(f) for f in ("d", "i")):
        return "refused"
    return "same" if _readable(entry) else "unsupported"


# keripy 1.3.6's native inception: its version field carries no genus version, and its codes
# predate the current tables (`Y` for the version, `-I` for a list).
KERIPY_136 = "3fb88ce9007d46415099972189cbaae784e14905"
KERIPY_136_INCEPTION = (
    "-FAtYKERICAAXicpEEx4oxGYbNrd6nZsdGu2KdN4MSDGD5IWS7hXjST7r8ewDG9X"
    "hvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-IALDG9XhvcVryHj"
    "oIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-IAAMAAA-IAA-IAA-IAA")


def _keripy_136_vectors():
    from keri.core import serdering
    try:
        serder = serdering.SerderKERI(raw=KERIPY_136_INCEPTION.encode())
        outcome = {"said": serder.said, "ilk": serder.ilk}
    except Exception as error:
        outcome = {"refused": type(error).__name__}
    return _numbered("event-keripy-1.3.6", [{
        "kind": "event", "input": {"raw": KERIPY_136_INCEPTION}, "keripy": outcome,
        "expect": "unsupported",
        "source": {**corpus.source("copied", "tests/core/test_serdering.py",
                                   "test_cesr_native_dumps"), "keripy": KERIPY_136},
    }])


def _stream_vectors():
    """Streams that carry a genus-version code before a message, read by keripy's parser: the
    code naming CESR 2.00, before the first message or between two, twice in a row, and naming
    other versions. Ed25519 signing is deterministic, so the bytes are the same on every run."""
    from keri.core import counting, eventing, parsing, signing
    from keri.core.coring import Diger, MtrDex
    from keri.db import basing
    from keri.kering import Kinds, Vrsn_2_0

    v2 = dict(version=Vrsn_2_0, kind=Kinds.cesr)
    keys = [signing.Signer(raw=hashlib.blake2b(f"genus-{n}".encode(), digest_size=32).digest(),
                           code=MtrDex.Ed25519_Seed, transferable=True) for n in range(3)]
    icp = eventing.incept(keys=[keys[0].verfer.qb64], ndigs=[Diger(ser=keys[1].verfer.qb64b).qb64],
                          code=MtrDex.Blake3_256, **v2)
    rot = eventing.rotate(pre=icp.pre, keys=[keys[1].verfer.qb64], dig=icp.said, sn=1,
                          ndigs=[Diger(ser=keys[2].verfer.qb64b).qb64], **v2)
    first = eventing.messagize(icp, sigers=[keys[0].sign(icp.raw, index=0)]).decode()
    second = eventing.messagize(rot, sigers=[keys[1].sign(rot.raw, index=0, ondex=0)]).decode()

    def genus(digits):
        return counting.Counter(code=counting.CtrDex_2_0.KERIACDCGenusVersion, countB64=digits,
                                version=Vrsn_2_0).qb64

    streams = {
        "test_genus_version_before_the_first_message": genus("CAA") + first + second,
        "test_genus_version_between_messages": first + genus("CAA") + second,
        "test_genus_version_twice_in_a_row": genus("CAA") + genus("CAA") + first + second,
        "test_genus_version_of_cesr_1_00": genus("BAA") + first + second,
        "test_genus_version_of_cesr_2_01": genus("CAB") + first + second,
        "test_genus_version_of_cesr_1_00_between_messages": first + genus("BAA") + second,
    }
    vectors = []
    for test, stream in streams.items():
        with basing.openDB(name="stream", temp=True) as db:
            kevery = eventing.Kevery(db=db, lax=False, local=False)
            parsing.Parser(version=Vrsn_2_0).parse(ims=bytearray(stream.encode()), kvy=kevery)
            kever = kevery.kevers.get(icp.pre)
            outcome = {"sn": None if kever is None else kever.sn}
        vectors.append({"kind": "stream", "input": {"stream": stream}, "keripy": outcome,
                        "expect": "same",
                        "source": corpus.source("generated", "crates/kerita/oracle/gen_harvest.py",
                                                test)})
    return _numbered("stream", vectors)


def _threshold_ok(sith):
    """A count, or one or more clauses of fractions; a nested weight map is not."""
    if isinstance(sith, (str, int)):
        return True
    clauses = sith if sith and isinstance(sith[0], list) else [sith]
    return all(isinstance(w, str) for clause in clauses for w in clause)


def _step_in_subset(step):
    sad = step["sad"]
    if step["event"] is None:
        return False
    traits = set(sad.get("c", []))
    allowed = corpus.TRAITS if sad.get("t") in ("icp", "dip") else set()
    sigs = [*step["sigs"], *step["wigs"]]
    return (step["event"].startswith("-F") and sad.get("v", "").startswith("KERICAACAA")
            and sad.get("t") in ILKS and _sad_codes(sad) <= corpus.MATTER and _seals_ok(sad)
            and traits <= allowed
            and all(_threshold_ok(sad[f]) for f in ("kt", "nt") if f in sad)
            and all(_indexer_code(s) in corpus.INDEXER for s in sigs))


def _indexer_code(qb64):
    from keri.core import indexing
    try:
        return indexing.Indexer(qb64=qb64).code
    except Exception:
        return None


# keripy's rules for a validator's own events that reach it from another source: the crate
# reads no event's source, which its caller's transport knows.
SOURCE_RULES = {"MisfitEventSourceError", "MissingDelegableApprovalError"}


def _kel_unsupported(steps):
    """Why the crate cannot replay a validator's steps, or None."""
    accepted = {}
    for n, step in enumerate(steps):
        if step["outcome"] in SOURCE_RULES:
            return f"step {n}: outside: keripy's rule for its own events from another source"
        if not _step_in_subset(step):
            return f"step {n}: an event or attachment outside the subset"
        pre, sn = step["sad"].get("i"), int(step["sad"].get("s", "0"), 16)
        if step["outcome"] == "accepted":
            if pre in accepted and sn <= accepted[pre]:
                return f"step {n}: superseding recovery"
            accepted[pre] = sn
    return None


def _kel_vectors(recorded, how):
    vectors = []
    for node in sorted(recorded):
        by_validator = {}
        for k, step in recorded[node]:
            if k == "kel":
                by_validator.setdefault(step["kevery"], []).append(step)
        for label, steps in sorted(by_validator.items()):
            if any(s["event"] is None for s in steps):
                continue
            why = _kel_unsupported(steps)
            vectors.append({
                "kind": "kel",
                "input": {"steps": [{f: s.get(f) for f in ("event", "sigs", "wigs", "delegation",
                                                           "message", "validator")}
                                    for s in steps]},
                "keripy": {"steps": [{"outcome": s["outcome"], "state": s["state"]}
                                     for s in steps]},
                "expect": "unsupported" if why else "same",
                **({"why": why} if why else {}),
                "source": corpus.source(how, *_where(node), validator=label),
            })
    return _numbered(f"kel-{how}", vectors)


def generate():
    """{file name: [vector]} for the keripy directory of the corpus, and the tests that failed."""
    copied, failed_copied = records(FILES)
    generated, failed_generated = records(["test_adr_cases.py", "test_refusals.py"],
                                          rootdir=os.path.dirname(os.path.abspath(__file__)))
    replayed, failed_replayed = records(gen_replayed.prepare(), rootdir=gen_replayed.ROOT)
    recorded = {**copied, **generated}
    failed = [*failed_copied, *failed_generated, *failed_replayed]
    return {
        "matter.jsonl": _primitive_vectors(
            recorded, "matter", lambda e: {"code": e["code"], "raw": e["raw"]},
            lambda e: e["code"] in corpus.MATTER),
        "indexer.jsonl": _primitive_vectors(
            recorded, "indexer",
            lambda e: {"code": e["code"], "index": e["index"], "ondex": e["ondex"], "raw": e["raw"]},
            lambda e: e["code"] in corpus.INDEXER),
        # A lone count code carries no version, and the crate reads it as CESR 2.00: its
        # code alone says whether the crate reads it.
        "counter.jsonl": _primitive_vectors(
            recorded, "counter",
            lambda e: {"code": e["code"], "count": e["count"], "version": e["version"]},
            lambda e: e["code"] in corpus.COUNTER),
        "event.jsonl": _event_vectors(recorded),
        "event-keripy-1.3.6.jsonl": _keripy_136_vectors(),
        "kel.jsonl": _kel_vectors(copied, "copied"),
        "kel-generated.jsonl": _kel_vectors(generated, "generated"),
        "kel-replayed.jsonl": _kel_vectors(replayed, "replayed"),
        "stream.jsonl": _stream_vectors(),
    }, failed
