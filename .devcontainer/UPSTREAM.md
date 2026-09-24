# Upstream syncs

What has been taken from sandcat into this devcontainer, sync by sync, and what was decided each time. Upstream is <https://github.com/VirtusLab/sandcat>, branch `master`. Newest sync first; the bottom entry is the baseline the devcontainer was first copied from.

Sandcat does not ship a devcontainer to copy. Its command-line tool, `sandcat init`, writes one from templates for a single agent and a set of options. `sandcat/` here is that output for Claude with the `no-rtk` feature, copied unchanged, so a sync is a fresh render and a diff. Everything this workspace adds lives in files sandcat does not own: `Dockerfile.app`, `compose-all.yml`, `devcontainer.json`, `scripts/project-init.sh`, `scripts/project-user-init.sh`, `codex/config.toml`, and the project's `.sandcat/settings.json`.

## How to sync

1. Clone upstream outside the workspace and read what changed in the templates since the last synced commit: `git log <hash>..origin/master -- cli/templates cli/lib`. The hash is the one in the newest entry below.
2. Render into an empty directory with a throwaway home directory, since `sandcat init` creates `~/.config/sandcat/settings.json` and agent config files when they are missing: `HOME=$(mktemp -d) bash cli/bin/sandcat init --name mee-pdn --path <empty dir> --agent claude --ide vscode --stacks rust --sp none --features no-rtk`.
3. Copy the render's `.devcontainer/sandcat/` over `sandcat/` here, leaving out the files this workspace does not use: `scripts/ensure-cache-volumes.sh` and the Codex, Copilot and Cursor addons (`mitmproxy_addon_codex.py`, `mitmproxy_addon_copilot.py`, `mitmproxy_addon_cursor.py`). `diff -r <render>/.devcontainer/sandcat .devcontainer/sandcat` then lists only those files.
4. Compare the render's `Dockerfile.app`, `compose-all.yml` and `devcontainer.json` with the ones here and carry over what applies; the list of standing differences below says what never does.
5. Check that the compose files still merge the way this workspace needs: `docker compose -f .devcontainer/compose-all.yml config`. The agent must mount `mitmproxy-public` and not `mitmproxy-config`, the mitmweb port must be bound to `127.0.0.1`, and the home volume must be named `mee-pdn-home`.
6. Rebuild the container and add an entry here: the upstream commit synced to, what was taken, and what was decided. An entry whose changes an existing home volume would hide ends with the line **Breaking — action required** and the steps to take.

## Standing differences

These hold across syncs; an entry below records only what changed in a sync.

- **Two agents in one image.** Sandcat renders one agent per project; this image carries Claude Code and Codex. mitmproxy loads the Claude addon. The Codex addon is the same class without overrides, so it would behave identically.
- **mise and apt instead of devbox.** The Rust toolchain comes from rustup, so `rust-toolchain.toml` selects the channel, components and wasm targets here as on every other machine; Rust from the Nix packages devbox installs reads no such file. The command-line utilities sandcat installs through devbox come from apt. The devbox home snapshot that sandcat's `app-init.sh` copies into the home volume on a changed image does not exist here, so that step does nothing.
- **Docker inside the container.** The Docker CLI and the host's Docker socket are there for the container tests and the demo. `scripts/project-init.sh` adds vscode to the group that owns the socket. This is the one way around the sandbox; the Security section of `README.md` says what it opens.
- **Build output in volumes.** `target/` and `mee-v3-single-device/target/` are volumes rather than the host's directories, and `scripts/project-init.sh` gives them to vscode.
- **Authentication.** Claude Code authenticates with `CLAUDE_CODE_OAUTH_TOKEN`, while sandcat seeds onboarding for `ANTHROPIC_API_KEY` only. The team has no API access and works within the limits of its Claude subscriptions, which an OAuth token from `claude setup-token` uses and an API key does not. `scripts/project-user-init.sh` also registers `gh` as git's credential helper and stores the `OPENAI_API_KEY` placeholder in Codex's auth cache.
- **Toolchain install on start.** `scripts/project-user-init.sh` installs the pinned Rust toolchain on every start, since the home volume outlives the image.
- **mitmweb on loopback.** Sandcat publishes the mitmweb UI on a dynamic port of every host interface, behind the fixed password `mitmproxy`. `compose-all.yml` binds it to `127.0.0.1:8081` instead.
- **Home volume name.** The home volume is `mee-pdn-home`, a name fixed before sandcat renamed its volume from `app-home` to `agent-home`; renaming it would lose the logins and history kept in it.
- **Not used:** rtk, the shared dependency-cache volumes for JVM builds and the `initializeCommand` that creates them, and the devbox files.

## 2026-09-24 · synced to `a85656c` · fix(installer): read the overwrite prompt from the terminal when piped into a shell (#123)

**Synced** from the `6da259d` baseline, across 69 upstream commits. The devcontainer is restructured to the form above: until this sync the workspace's changes sat inside sandcat's own files, and a sync meant reading them apart by hand.

Taken as upstream has them:

- The agent container mounts only the new `mitmproxy-public` volume, which holds the proxy's public CA certificate and the generated environment file (#97). Until now it mounted the whole `mitmproxy-config` volume read-only, and with it the CA private key and the WireGuard private keys, so code in the sandbox could read the key that signs every certificate the proxy presents.
- The image removes vscode's passwordless sudo (#103), and the agent container runs with `no-new-privileges` (#90). Nothing here used sudo: every root step runs in the entrypoint before it drops to vscode.
- DNS goes through a dnsmasq forwarder inside `wg-client` (`ebd3430`), and Docker's embedded resolver forwards to an unroutable address. Before, a name under the compose network's search domain could be resolved by the host's resolver, outside the proxy, which made it a channel for leaking data.
- The generated environment file, which every shell in the container sources, quotes its values with `shlex.quote` instead of a hand-written escape (#101). Its values are the `env` entries of the settings and the placeholders; secret values never reach it.
- A host `.gitconfig` that VS Code copies in anyway is removed at start (#95).
- The mitmproxy image is pinned to `12.2.3` instead of `latest` (#100).
- The healthchecks have start periods for slow cold starts (#74), `wg-client` restarts after a crash, and the healthcheck waits for the CA certificate (`051ee9a`).
- The mitmproxy addon is split into a shared library and a thin addon per agent. The library substitutes placeholders inside Basic auth headers (#85), which covers what this workspace's own patch of the addon did; the patch is gone.
- The proxy reads a third settings layer, `.sandcat/settings.local.json`, now gitignored here. It also supports network presets (#105), `extra_hosts` (#55), custom DNS servers (`fe0f4b9`) and extra upstream CA bundles (#92); none of them is used yet.
- The agent's constant compose settings moved to `sandcat/compose-agent.yml` (#99); `compose-all.yml` holds only what this workspace sets.
- The entrypoint exports the CA bundle in the variables common TLS clients read (`SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `GIT_SSL_CAINFO`) and makes Node.js use the system store. This covers Codex, so `CODEX_CA_CERTIFICATE` is no longer set.
- The image sets `LANG` to `en_US.UTF-8` (#67).

Adapted:

- Codex is installed by its native installer into `/usr/local/bin`, as sandcat does for its Codex agent (#87), instead of from npm into the home volume, where a rebuild never updated it.
- The workspace's steps moved out of sandcat's `app-init.sh` and `app-user-init.sh` into `scripts/project-init.sh`, which is the image entrypoint and runs sandcat's, and `scripts/project-user-init.sh`, which sandcat's entrypoint runs as vscode before the container command. Sandcat now marks every directory under `/workspaces` as a safe git directory itself, so that line is gone from ours.

Not adopted:

- devbox with Nix in place of mise (#80): see the standing differences.
- rtk, which rewrites the output of shell commands before the agent reads it (#83). Raw output of test and stress runs matters when a flaky test is diagnosed, and the hook patches the agent's settings on its own.
- The shared dependency-cache volumes (#81) and the `initializeCommand` that creates them (#122): nothing here builds with the JVM.
- The dynamic mitmweb port on every host interface: see the standing differences.

**Breaking — action required:** stop the devcontainer, remove the `mee-pdn-home` volume (`docker volume rm mee-pdn-home`), rebuild the devcontainer.

## 2026-03-27 · `6da259d` · Fix Dockerfile ampersand escaping under Bash compat32 mode

**The baseline**, reconstructed on 2026-09-24: the commit that first added the devcontainer, `ccc882a` on 2026-04-06, recorded no upstream revision. The files copied then match upstream's templates equally well at every commit from `c6797d6` to `6da259d`, apart from this workspace's own changes, and the entry takes the last of them. `c6797d6` is the first to install fd, fzf and ripgrep, which the copy has, and the commit after `6da259d` renames the home volume, which the copy does not. `wg-client-init.sh`, `Dockerfile.wg-client`, `app-post-start.sh` and `tmux.conf` match byte for byte.
