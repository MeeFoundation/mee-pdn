"""The corpora of cesr-rs, admitted when keripy reproduces them.

A vector of cesr-rs is one implementation's word: it enters the crate's corpus
only when keripy at the pin, given the same text, gives the same code, value
and binary form. One it does not reproduce stays out, listed with both outputs.
"""
import json
import os

from keri.core import coring, counting, indexing
from keri.kering import Vrsn_2_0

import corpus

HERE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "cesr-rs")
UPSTREAM = "crates/cesr-stream/tests/corpus/keripy"


def _matter(v):
    m = coring.Matter(qb64=v["qb64"])
    return {"code": m.code, "raw": m.raw.hex(), "qb2": m.qb2.hex()}


def _indexer(v):
    i = indexing.Indexer(qb64=v["qb64"])
    return {"code": i.code, "index": i.index, "ondex": i.ondex, "raw": i.raw.hex(),
            "qb2": i.qb2.hex()}


def _counter(v):
    c = counting.Counter(qb64=v["qb64"], version=Vrsn_2_0)
    return {"code": c.code, "count": c.count, "qb2": c.qb2.hex()}


FAMILIES = {
    "matter.jsonl": ("matter", _matter, corpus.MATTER),
    "indexer.jsonl": ("indexer", _indexer, corpus.INDEXER),
    "counter_v2.jsonl": ("counter", _counter, corpus.COUNTER),
}


def _theirs(vector, ours):
    """What the cesr-rs vector says, in the fields keripy's answer has.

    cesr-rs labels a big counter (`--C`) by its small code (`-C`); its bytes and its count
    are what the label stands for, so the label keripy gives is taken when those agree.
    """
    theirs = {k: vector.get(k) for k in ours}
    if vector.get("kind") == "counter" and vector["qb64"].startswith("--") \
            and ours["code"] == "-" + vector["code"]:
        theirs["code"] = ours["code"]
    return theirs


def generate():
    """({file name: [vector]}, [rejected]) for the cesr-rs directory of the corpus."""
    with open(os.path.join(HERE, "CESR_RS_PIN"), encoding="ascii") as f:
        pin = f.read().strip()
    files, rejected = {}, []
    for name, (kind, reproduce, subset) in FAMILIES.items():
        admitted = []
        with open(os.path.join(HERE, name), encoding="ascii") as f:
            lines = [json.loads(line) for line in f]
        for n, vector in enumerate(lines, start=1):
            where = {"file": f"{UPSTREAM}/{name}", "line": n, "cesr_rs": pin}
            try:
                ours = reproduce(vector)
            except Exception as error:
                rejected.append({**where, "cesr-rs": vector, "keripy": {"refused": type(error).__name__}})
                continue
            if ours != _theirs(vector, ours):
                rejected.append({**where, "cesr-rs": vector, "keripy": ours})
                continue
            admitted.append({
                "kind": kind, "input": {"qb64": vector["qb64"]}, "keripy": ours,
                "expect": "same" if ours["code"] in subset else "unsupported",
                "source": {**corpus.source("cesr-rs", f"{UPSTREAM}/{name}", f"line {n}"),
                           "cesr_rs": pin},
            })
        for n, v in enumerate(admitted):
            v["id"] = f"cesr-rs-{kind}/{n}"
        files[name.replace("_v2", "")] = admitted
    return files, rejected
