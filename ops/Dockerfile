# The stand's node: one `pdn-node-http` binary, one embedded runtime.
#
# The build is release — the stand's subject is sync under real timing, and a
# debug node under that timing is a different animal from the one the demo
# shows. Dependencies are compiled in their own layer (cargo-chef), so a
# source edit rebuilds the workspace alone.

FROM rust:1.93-bookworm AS chef
RUN cargo install cargo-chef --locked --version ^0.1
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# A workspace may resolve the store fork from the checkout beside it, through
# a cargo configuration file kept out of version control. Neither is present
# in a clean checkout, and a `COPY` of a path that is not there fails the
# build, so this stage turns "may be present" into "is present, possibly
# empty" — and the dependency stage below then resolves exactly what the
# final build does, rather than reaching for a checkout it was never given.
FROM busybox AS local-inputs
COPY . /ctx
RUN mkdir -p /out/.cargo /out/pdn-store; \
    cp -a /ctx/.cargo/. /out/.cargo/ 2>/dev/null || true; \
    cp -a /ctx/pdn-store/. /out/pdn-store/ 2>/dev/null || true

FROM chef AS builder
COPY --from=local-inputs /out/.cargo ./.cargo
COPY --from=local-inputs /out/pdn-store ./pdn-store
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release -p pdn-node-http --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release -p pdn-node-http

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 pdn
COPY --from=builder /app/target/release/pdn-node-http /usr/local/bin/pdn-node-http
USER pdn

# The HTTP surface listens on every interface, so a caller outside the
# container reaches it through a published port. The runtime's endpoint bind
# stays unset on purpose: unset, the endpoint binds every interface and
# publishes the container's own address, which is the address a peer
# container dials. Set it and the node publishes an address only itself can
# reach.
ENV PDN_HOST=0.0.0.0 \
    PDN_PORT=3011
EXPOSE 3011

# The debug subtree stays absent unless the caller asks for it: the stand
# passes `PDN_DEBUG=1`, and the payloads that subtree carries are live
# ceremony secrets.
ENTRYPOINT ["/usr/local/bin/pdn-node-http"]
