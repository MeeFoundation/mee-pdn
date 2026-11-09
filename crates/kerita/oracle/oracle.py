#!/usr/bin/env -S python3 -I
"""The KERI oracle's entry point, run only inside the oracle's image.

  oracle.py info                    keripy's commit and version
  oracle.py corpus <kerita> <cesr>  regenerate kerita's corpus under <kerita>, kerita-cesr's
                                    under <cesr>
  oracle.py fold <dir>              fold the KELs the crate wrote under <dir> in keripy
  oracle.py harvest <out> <root> <file>...
                                    run test files under <root>, write what they built to <out>
"""
import sys

if not sys.flags.isolated:
    sys.exit("run as python3 -I")

import json  # noqa: E402
import os  # noqa: E402

# The image holds nothing in /opt/oracle but the generators, so its own
# directory is safe to import from.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def info():
    import keri

    print(f"keripy {keri.__version__} at {os.environ['KERIPY_PIN']}")


def harvest(out, rootdir, paths):
    import harvest as harvesting

    records, failed = harvesting.run(paths, rootdir)
    with open(out, "w", encoding="ascii") as f:
        json.dump({"records": records, "failed": failed}, f)


def regenerate(events, primitives):
    """Writes the vectors of events, messages, KELs and streams under `events`, kerita's corpus,
    and those of primitives under `primitives`, kerita-cesr's; one harvest feeds both, so each
    names the keripy tests that failed."""
    import corpus
    import gen_cesr_rs
    import gen_harvest

    files, failed = gen_harvest.generate()
    for directory, names in ((events, sorted(set(files) - corpus.PRIMITIVE_FILES)),
                             (primitives, sorted(corpus.PRIMITIVE_FILES))):
        corpus.write(os.path.join(directory, "keripy"), {n: files[n] for n in names})
        corpus.write_failed(os.path.join(directory, "keripy", "FAILED"), failed)
    admitted, rejected = gen_cesr_rs.generate()
    corpus.write(os.path.join(primitives, "cesr-rs"), admitted)
    corpus.write_rejected(os.path.join(primitives, "cesr-rs", "REJECTED.jsonl"), rejected)


def fold(directory):
    """Folds the KELs the crate wrote in keripy, recording keripy's answer to each message."""
    import harvest as harvesting
    from keri.core import eventing, parsing
    from keri.db import basing
    from keri.kering import Vrsn_2_0

    import corpus

    harvest = harvesting.Harvest()
    harvesting.install(harvest)
    path = os.path.join(directory, "kels.jsonl")
    with open(path, encoding="ascii") as f:
        vectors = [json.loads(line) for line in f]
    for vector in vectors:
        harvest.current = vector["id"]
        steps = []
        with basing.openDB(name=vector["id"].replace("/", "-"), temp=True) as db:
            kevery = eventing.Kevery(db=db, lax=False, local=False)
            for step in vector["input"]["steps"]:
                harvest.pending = []
                parsing.Parser(version=Vrsn_2_0).parse(
                    ims=bytearray(step["message"].encode()), kvy=kevery)
                given = [e for kind, e in harvest.pending if kind == "kel"]
                steps.append({"outcome": given[-1]["outcome"], "state": given[-1]["state"]}
                             if given else {"outcome": "ParseError", "state": None})
        vector["keripy"] = {"steps": steps}
        vector["source"]["keripy"] = corpus.PIN
    corpus.write(directory, {"kels.jsonl": vectors})


def main(argv):
    match argv:
        case ["info"]:
            info()
        case ["corpus", events, primitives]:
            regenerate(events, primitives)
        case ["fold", directory]:
            fold(directory)
        case ["harvest", out, rootdir, *paths] if paths:
            harvest(out, rootdir, paths)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
