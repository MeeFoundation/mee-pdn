# The KERI oracle: keripy at the commit KERIPY_PIN names, and the generators that
# build the corpora of kerita and kerita-cesr from it (crates/kerita/CLAUDE.md). Built and run by
# scripts/keri-oracle.sh, never by hand. The base image by digest and the Python
# packages by constraints.txt, so that a night rebuilds the oracle that the pin
# named: python:3.14-slim-trixie at 3.14.10.
FROM python:3.14-slim-trixie@sha256:930e1abba7c4e8f96ed93e8012c35d174d7732752731bbd5e8e757d7ec6f3966

ARG KERIPY_PIN
# Before anything is fetched: a pin is a full commit, never a branch or a tag,
# so a byte the oracle disagrees on is unambiguous.
RUN printf '%s' "$KERIPY_PIN" | grep -qxE '[0-9a-f]{40}' \
    || { echo "KERIPY_PIN is not a full commit hash: '$KERIPY_PIN'" >&2; exit 1; }

# libsodium for pysodium, a compiler for keripy's native dependencies.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libsodium23 git ca-certificates build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY constraints.txt /opt/oracle/constraints.txt
RUN git init -q /opt/keripy \
    && git -C /opt/keripy remote add origin https://github.com/WebOfTrust/keripy \
    && git -C /opt/keripy fetch -q --depth 1 origin "$KERIPY_PIN" \
    && git -C /opt/keripy checkout -q FETCH_HEAD \
    && test "$(git -C /opt/keripy rev-parse HEAD)" = "$KERIPY_PIN" \
    && pip install --no-cache-dir -q -c /opt/oracle/constraints.txt "/opt/keripy[test]"

ENV KERIPY_PIN=${KERIPY_PIN}
COPY *.py /opt/oracle/
# The corpora of cesr-rs at CESR_RS_PIN, verbatim; gen_cesr_rs.py admits what keripy reproduces.
COPY cesr-rs /opt/oracle/cesr-rs
WORKDIR /work
ENTRYPOINT ["python3", "-I", "/opt/oracle/oracle.py"]
