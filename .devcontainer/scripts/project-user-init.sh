#!/bin/bash
#
# This workspace's vscode-phase steps, after sandcat's app-init.sh has trusted
# the proxy CA and exported sandcat.env. A failed step leaves the container up.
#

# git sends the GITHUB_TOKEN placeholder in Basic auth; mitmproxy swaps in the
# real token for the hosts the secret names.
if [ -n "${GITHUB_TOKEN:-}" ]; then
    gh auth setup-git
fi

# sandcat seeds the onboarding flag for ANTHROPIC_API_KEY only.
if [ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]; then
    echo '{"hasCompletedOnboarding":true}' > "$HOME/.claude.json"
fi

# Codex's persistent auth cache holds the placeholder, never the key.
if [ -n "${OPENAI_API_KEY:-}" ] && ! codex login status >/dev/null 2>&1; then
    printf '%s' "$OPENAI_API_KEY" | codex login --with-api-key >/dev/null \
        || echo "codex login failed" >&2
fi

# VS Code's credential helper is off (devcontainer.json); a credsStore it wrote
# before names a helper that no longer exists, and every pull fails on it.
DOCKER_CONFIG_FILE="$HOME/.docker/config.json"
if [ -f "$DOCKER_CONFIG_FILE" ] \
    && jq -e '(.credsStore // "") | startswith("dev-containers-")' "$DOCKER_CONFIG_FILE" >/dev/null 2>&1; then
    jq 'del(.credsStore)' "$DOCKER_CONFIG_FILE" > "$DOCKER_CONFIG_FILE.tmp" \
        && mv "$DOCKER_CONFIG_FILE.tmp" "$DOCKER_CONFIG_FILE" \
        || echo "could not remove VS Code's credsStore from $DOCKER_CONFIG_FILE" >&2
fi

# On every start, since the home volume outlives the image. rustup's own binary:
# the mise shim sets RUSTUP_TOOLCHAIN, and rustup then ignores rust-toolchain.toml.
(cd /workspaces/mee-pdn && "$HOME/.cargo/bin/rustup" toolchain install) \
    || echo "rust toolchain install failed; in the container run: just setup-tooling" >&2

exec "$@"
