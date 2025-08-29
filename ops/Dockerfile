
# source: https://depot.dev/blog/rust-dockerfile-best-practices
FROM rust:1.89 AS base

RUN cargo install sccache --version ^0.10
RUN cargo install cargo-chef --version ^0.1
ENV RUSTC_WRAPPER=sccache SCCACHE_DIR=/sccache

FROM base AS planner

WORKDIR /app
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
  cargo chef prepare --recipe-path recipe.json

FROM base AS builder

WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
  cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
  cargo build --release -p mee-node-axum

FROM debian:bookworm-slim AS runtime

RUN useradd -m -u 10001 appuser \
  && apt-get update && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/mee-node-axum /usr/local/bin/mee-node-axum

USER appuser

EXPOSE 3000

# Defaults for quick local runs; override as needed
ENV MEE_PROFILE=node \
    MEE_HOST=0.0.0.0 \
    MEE_PORT=3000 \
    MEE_BASE_URL=http://localhost:3000

ENTRYPOINT ["/usr/local/bin/mee-node-axum"]
CMD ["--profile","node","--addr","0.0.0.0:3000","--base-url","http://localhost:3000"]
