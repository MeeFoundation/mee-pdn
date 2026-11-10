"""The corpus format, and the subset the crates read (crates/kerita/CLAUDE.md).

A corpus file holds JSON lines, one vector each:

  {"id": ..., "source": {"keripy": <commit>, "how": copied|replayed|generated|cesr-rs,
   "file": ..., "test": ...}, "kind": ..., "input": ..., "keripy": ..., "expect": same|unsupported}

Each directory's MANIFEST lists its files with their vector counts, so a file
emptied by mistake fails the corpus check instead of passing as zero cases.
"""
import json
import os

PIN = os.environ["KERIPY_PIN"]

# Keys, the digest and signatures of the subset, then the structural codes a
# native body is built from: numbers, tags, Base64 strings for weights.
MATTER = {
    "D", "B", "1AAJ", "1AAI",
    "E",
    "0B", "0I",
    "M", "0H", "R", "N", "S", "T", "0A", "U",
    "0J", "0K", "X", "1AAF", "0L", "0M", "Y", "1AAN", "0N", "0O", "Z",
    "4A", "5A", "6A", "7AAA", "8AAA", "9AAA",
}
INDEXER = {"A", "B", "E", "F", "2A", "2B", "2E", "2F"}
COUNTER = {
    "-B", "--B", "-C", "--C", "-F", "--F", "-J", "--J", "-K", "--K", "-L", "--L",
    "-M", "--M", "-Q", "--Q", "-S", "--S", "-T", "--T", "-_AAA",
}
TRAITS = {"EO"}

# The files of keripy's vectors kerita-cesr's corpus holds; the rest are kerita's.
PRIMITIVE_FILES = {"matter.jsonl", "indexer.jsonl", "counter.jsonl"}


def source(how, file, test, **extra):
    return {"keripy": PIN, "how": how, "file": file, "test": test, **extra}


def write(directory, files):
    """Writes {name: [vector]} under directory, then its MANIFEST, each file whole."""
    os.makedirs(directory, exist_ok=True)
    for name, vectors in sorted(files.items()):
        _replace(os.path.join(directory, name),
                 "".join(json.dumps(v, sort_keys=True) + "\n" for v in vectors))
    manifest = "".join(f"{name} {len(vectors)}\n" for name, vectors in sorted(files.items()))
    _replace(os.path.join(directory, "MANIFEST"), manifest)


def write_rejected(path, rejected):
    """Vectors of another implementation keripy does not reproduce, with both outputs; no
    manifest lists this file, so the corpus tests never read it as vectors."""
    _replace(path, "".join(json.dumps(r, sort_keys=True) + "\n" for r in rejected))


def write_failed(path, nodes):
    """keripy's tests that failed at the pin, one node id a line: what they built is missing
    from the corpus, which this file then says; no manifest lists it."""
    _replace(path, "".join(f"{node}\n" for node in nodes))


def _replace(path, text):
    """A file is replaced whole, so a cut run leaves the previous one."""
    temporary = path + ".tmp"
    with open(temporary, "w", encoding="ascii") as out:
        out.write(text)
    os.replace(temporary, path)
