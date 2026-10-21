# The load test in one container: the observation build of pdn-node-http and the runner that starts N of them.
# The node image comes from the mee-pdn workspace (`just build-observation-image`); container.sh builds both.
ARG NODE_IMAGE=pdn-node-http:observation
FROM ${NODE_IMAGE}

# procps for pgrep, elfutils for a hung node's stack (eu-stack).
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 procps elfutils \
    && rm -rf /var/lib/apt/lists/*

COPY load.py analyze.py /opt/sync-test/
ARG REVISION=unknown
ENV PDN_REVISION=${REVISION}

# Results go to /out, which the caller mounts; the nodes' stores stay on the container's own disk, since a mounted
# host directory (Docker Desktop above all) would slow the store and the numbers with it.
WORKDIR /out
ENTRYPOINT ["python3", "-I", "/opt/sync-test/load.py", "--bin", "/usr/local/bin/pdn-node-http", "--out", "/out", \
            "--nodes-dir", "/var/tmp/pdn-nodes"]
CMD ["--help"]
