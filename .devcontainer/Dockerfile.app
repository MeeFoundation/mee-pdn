FROM mcr.microsoft.com/devcontainers/base:debian

# ca-certificates, curl, git are already in the devcontainers base image.
# fd-find:  fast file finder (aliased to fd below)
# fzf:      fuzzy finder for files and command history
# gh:       GitHub CLI
# gosu:     drops privileges in the entrypoint
# jq:       JSON processor
# python3:  runs the repository's scripts/*.py
# ripgrep:  fast recursive grep (rg)
# tmux:     terminal multiplexer
# vim:      text editor
RUN apt-get update \
    && apt-get install -y --no-install-recommends fd-find fzf gh gosu jq python3 ripgrep tmux vim \
    && ln -s $(which fdfind) /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# Docker CLI from the official repository: testcontainers drives the daemon
# through it, and the demo brings its nodes up with the compose plugin.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian trixie stable" > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli docker-buildx-plugin docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/*

# Every root step runs in the entrypoint before it drops to vscode, so vscode
# needs no sudo; compose-agent.yml's no-new-privileges is the runtime half.
RUN rm -f /etc/sudoers.d/vscode

COPY --chmod=755 sandcat/scripts/app-init.sh /usr/local/bin/app-init.sh
COPY --chmod=755 sandcat/scripts/app-user-init.sh /usr/local/bin/app-user-init.sh
COPY --chmod=644 sandcat/scripts/java-env.sh /etc/profile.d/sandcat-java.sh
COPY --chmod=755 scripts/project-init.sh /usr/local/bin/project-init.sh
COPY --chmod=755 scripts/project-user-init.sh /usr/local/bin/project-user-init.sh
COPY --chown=vscode:vscode sandcat/tmux.conf /home/vscode/.tmux.conf
COPY codex/config.toml /etc/codex/config.toml

RUN groupadd -f docker \
    && usermod -aG docker vscode

# Outside the home volume, so a rebuild upgrades it. CODEX_HOME applies to the
# installer only; at runtime Codex reads ~/.codex.
RUN curl -fsSL https://chatgpt.com/codex/install.sh | \
    CODEX_INSTALL_DIR=/usr/local/bin CODEX_HOME=/opt/codex-home sh

USER vscode

ENV LANG="en_US.UTF-8"

# Install Claude Code (native binary — no Node.js required).
RUN curl -fsSL https://claude.ai/install.sh | bash

# Install mise (SDK manager) for language toolchains.
RUN curl https://mise.run | sh
# Make mise available in login shells (su - vscode) and Docker CMD/RUN.
RUN echo 'export PATH="/home/vscode/.local/bin:/home/vscode/.local/share/mise/shims:$PATH"' >> /home/vscode/.profile
ENV PATH="/home/vscode/.local/bin:/home/vscode/.local/share/mise/shims:$PATH"

# The versions tools.toml pins: this image builds from .devcontainer/ alone and
# cannot read it, so `just check` compares the two.
ARG JUST_VERSION=1.58.0
ARG CARGO_NEXTEST_VERSION=0.9.146
ARG CARGO_DENY_VERSION=0.20.2
ARG CARGO_WATCH_VERSION=8.5.3

# Install just command runner
RUN curl -fsSL https://just.systems/install.sh | bash -s -- --tag "$JUST_VERSION" --to ~/.local/bin

RUN mise use -g rust@latest

# The workspace's rust-toolchain.toml selects the toolchain here as it does for
# rustup elsewhere; without this mise would hold `rust@latest` over it.
RUN mise settings add idiomatic_version_file_enable_tools rust

# Install cargo-nextest (pre-built, arch-aware) to ~/.local/bin (on PATH).
# Required by `just test` / `just stress`. Local machines: `just setup-tooling`.
RUN ARCH="$(uname -m)"; \
    case "$ARCH" in aarch64|arm64) NX=linux-arm ;; *) NX=linux ;; esac; \
    curl -LsSf "https://get.nexte.st/$CARGO_NEXTEST_VERSION/$NX" | tar zxf - -C /home/vscode/.local/bin

# Install cargo-deny (pre-built, arch-aware) to ~/.local/bin: `just audit`,
# the same deny.toml check the nightly dependency-audit job runs.
RUN ARCH="$(uname -m)"; \
    case "$ARCH" in aarch64|arm64) T=aarch64-unknown-linux-musl ;; *) T=x86_64-unknown-linux-musl ;; esac; \
    TAG="$CARGO_DENY_VERSION"; \
    curl -fsSL "https://github.com/EmbarkStudios/cargo-deny/releases/download/$TAG/cargo-deny-$TAG-$T.tar.gz" \
    | tar zxf - --strip-components=1 -C /home/vscode/.local/bin "cargo-deny-$TAG-$T/cargo-deny"

# Install cargo-watch (pre-built, arch-aware): `just build-watch`. Outside the
# home volume, so a rebuild upgrades it.
USER root
RUN ARCH="$(uname -m)"; \
    case "$ARCH" in aarch64|arm64) T=aarch64-unknown-linux-gnu ;; *) T=x86_64-unknown-linux-gnu ;; esac; \
    TAG="v$CARGO_WATCH_VERSION"; \
    curl -fsSL "https://github.com/watchexec/cargo-watch/releases/download/$TAG/cargo-watch-$TAG-$T.tar.xz" \
    | tar Jxf - --strip-components=1 --no-same-owner -C /usr/local/bin "cargo-watch-$TAG-$T/cargo-watch"
USER vscode

# Node.js + OpenSpec CLI (used by the repo-scoped agent skills).
RUN mise use -g node@latest \
    && npm install -g @fission-ai/openspec

# Pre-create agent state directories so bind mounts and first-run setup do not
# create them as root-owned. Both live on the persistent home volume.
RUN mkdir -p /home/vscode/.claude /home/vscode/.codex

RUN echo 'alias claude-yolo="claude --dangerously-skip-permissions"' >> /home/vscode/.bashrc
RUN echo 'alias codex-yolo="codex --dangerously-bypass-approvals-and-sandbox"' >> /home/vscode/.bashrc

USER root
ENTRYPOINT ["/usr/local/bin/project-init.sh"]
