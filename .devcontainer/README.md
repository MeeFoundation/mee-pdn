# Agent Token Setup for Devcontainer

One-time setup on the host machine to work with devcontainers. The devcontainer runs in both VS Code and Zed.

The container is a [sandcat](https://github.com/VirtusLab/sandcat) sandbox: all of its traffic goes through a WireGuard tunnel into mitmproxy, which applies the network rules and puts the real secrets into requests in place of placeholders. `sandcat/` is sandcat's generated setup, copied unchanged; everything this workspace adds lives in `Dockerfile.app`, `compose-all.yml`, `devcontainer.json` and `scripts/`. [UPSTREAM.md](UPSTREAM.md) records each sync with sandcat, what was decided in it, and how to run the next one.

## Required Tokens

1. **GitHub Personal Access Token** — for cloning private repositories, push/pull, access to GitHub Packages
2. **Claude Code OAuth Token** — for using Claude Code with your Pro/Max subscription (no API billing)
3. **OpenAI API key** — optional non-interactive Codex authentication; use device login instead for ChatGPT subscription access

---

## 1. Obtaining GitHub Personal Access Token (classic)

### Steps:

1. Open https://github.com/settings/tokens/new
2. Fill in **Note** — descriptive name, e.g., `sandcat-devcontainer` or `mee-pdn-devcontainer`
3. Set **Expiration** — recommended: 90 days
4. Select scopes:
   - ✅ **repo** — full access to private repositories (clone, push, pull)
   - ✅ **workflow** — push commits that change `.github/workflows/`; without it GitHub rejects such a push
   - ✅ **read:packages** — read packages from GitHub Package Registry (if project uses it)
   - ✅ **read:org** — read organization membership (needed if repository is in an organization)
5. Click **Generate token**
6. **Copy the token immediately** — it will not be shown again!
   - Token format: `ghp_...` (40+ characters)
   - Save in a password manager

---

## 2. Obtaining Claude Code OAuth Token

### Steps:

1. On the **host machine** (NOT inside the container!) run:

   ```bash
   claude setup-token
   ```

2. Follow the browser authorization instructions:
   - Browser will open with authorization form
   - Log in to your Claude account (Pro or Max subscription required)
   - Allow access

3. After successful authorization, the token will appear in the terminal:

   ```
   Token: sk-ant-oat01-...
   ```

4. **Copy the token** — it's valid for 1 year and will not be shown again!
   - Token format: `sk-ant-oat01-...` (long string)
   - Save in a password manager

---

## 3. Obtaining an OpenAI API Key (optional)

Create a key at https://platform.openai.com/api-keys and save it in a password manager. API-key Codex usage is billed through the OpenAI Platform account.

To use ChatGPT subscription access instead, skip this secret. After the container starts, run:

```bash
codex login --device-auth
```

The login survives rebuilds in the `mee-pdn-home` Docker volume. Device-code login must be enabled for the ChatGPT account or workspace.

---

## 4. Creating Configuration File

### Where tokens are stored:

Tokens are stored in `~/.config/sandcat/settings.json` on the **host machine**.

mitmproxy merges three settings layers, highest precedence last: that user file, the project's `.sandcat/settings.json` (committed), and `.sandcat/settings.local.json` (gitignored, for rules of your own on this project). Network rules from all three are evaluated top to bottom, highest-precedence layer first; the first match wins and anything unmatched is denied.

Codex runs with full access inside the devcontainer by default (`sandbox_mode = "danger-full-access"`, `approval_policy = "never"`). Docker and Sandcat remain the outer filesystem and network security boundaries. This default is container-scoped and does not change the host Codex configuration.

### Creating configuration:

```bash
# Create directory (if it doesn't exist)
mkdir -p ~/.config/sandcat

# Create configuration file
cat > ~/.config/sandcat/settings.json << 'SETTINGS'
{
  "env": {
    "GIT_USER_NAME": "Your Name",
    "GIT_USER_EMAIL": "your@email.com"
  },
  "secrets": {
    "CLAUDE_CODE_OAUTH_TOKEN": {
      "value": "PASTE_YOUR_CLAUDE_OAUTH_TOKEN",
      "hosts": ["*.anthropic.com", "*.claude.ai", "*.claude.com"]
    },
    "OPENAI_API_KEY": {
      "value": "PASTE_YOUR_OPENAI_API_KEY",
      "hosts": ["api.openai.com"]
    },
    "GITHUB_TOKEN": {
      "value": "PASTE_YOUR_GITHUB_PAT",
      "hosts": ["github.com", "*.github.com", "*.githubusercontent.com"]
    }
  },
  "network": [
    {"action": "allow", "host": "*.github.com"},
    {"action": "allow", "host": "github.com"},
    {"action": "allow", "host": "*.githubusercontent.com"},
    {"action": "allow", "host": "*.anthropic.com"},
    {"action": "allow", "host": "*.claude.ai"},
    {"action": "allow", "host": "*.claude.com"},
    {"action": "allow", "host": "*.openai.com"},
    {"action": "allow", "host": "*.openai.org"},
    {"action": "allow", "host": "*.chatgpt.com"},
    {"action": "allow", "host": "chatgpt.com"}
  ]
}
SETTINGS
```

## Additional Information

### Security

- Tokens are stored only on the host machine in `~/.config/sandcat/settings.json`
- Inside the container, each token's environment variable holds a placeholder (`SANDCAT_PLACEHOLDER_GITHUB_TOKEN`), never the token
- Mitmproxy intercepts HTTP(S) requests and replaces placeholders with real tokens, only for the hosts the secret names; a placeholder on its way to any other host gets the request refused
- The replacement covers request bodies too. Text that quotes a placeholder and goes to one of the secret's hosts carries the real token: a pull request description or a comment sent through `gh` publishes the `GITHUB_TOKEN`, and a prompt to Claude sends it to Anthropic if the secret lists Anthropic's hosts. Never put a placeholder string into such text, and list only the hosts that need the token
- Tokens **are not logged** and **not saved in command history**
- The container gets only mitmproxy's public CA certificate and the generated environment file; the CA private key and the WireGuard keys stay in a volume it does not mount
- The mitmweb UI listens on `http://127.0.0.1:8081` of the host only (password `mitmproxy`)
- **The Docker socket is the one way around all of this.** It is mounted for the container tests. A container started through it runs on the host's daemon, outside the tunnel, the network rules and secret substitution, and it can mount any host directory — `~/.config/sandcat/settings.json` with the real tokens included. Anything running in the devcontainer can use it

### Token Expiration

- **GitHub PAT (classic)**: expiration set by you (recommended 90 days)
- **Claude OAuth**: valid for 1 year, after which you need to re-run `claude setup-token`
- **OpenAI API key**: valid until revoked; rotate it according to your organization's policy

### Replacing Tokens

If a token expires or is compromised:

1. Generate a new token (following instructions above)
2. Update the value in `~/.config/sandcat/settings.json`
3. Rebuild the devcontainer (in VS Code: **Dev Containers: Rebuild Container**)

---

## Where Tokens Are Used

### GITHUB_TOKEN

- `git clone` private repositories
- `git push` / `git pull`
- `gh` CLI (GitHub CLI)
- Access to GitHub Packages / Container Registry

Check:

```sh
gh repo view MeeFoundation/mia-docs --json name,visibility
```

### CLAUDE_CODE_OAUTH_TOKEN

- `claude` command inside devcontainer
- Claude Code agent (AI assistant)
- Uses your Pro/Max subscription instead of API billing

### OPENAI_API_KEY

- `codex` CLI and the Codex IDE extension inside the devcontainer
- Stored as a Sandcat placeholder in the container; mitmproxy substitutes the real key only for `api.openai.com`
- Uses OpenAI Platform billing. For ChatGPT subscription access, omit the key and use `codex login --device-auth`

Check:

```sh
codex --version
codex login status
```

### GIT_USER_NAME / GIT_USER_EMAIL

- Git commit author
- Automatically configured on container startup

---

## Troubleshooting

### Build-time installs from `Dockerfile.app` not visible after rebuild

The `/home/vscode` directory is backed by the named Docker volume `mee-pdn-home` so Claude/Codex auth, shell history, and similar user state survive rebuilds. The trade-off: the volume is populated from the image only on **first** container creation. Subsequent rebuilds keep the existing volume contents, so anything new the Dockerfile installs into `/home/vscode/...` (mise toolchains, npm globals, just, cargo-nextest, cargo-deny) is shadowed. Codex, the Docker CLI and the apt packages live outside the home directory, and a rebuild does update them.

Symptom: `openspec --version` (or another tool just added to `Dockerfile.app`) returns `command not found` after **Rebuild Container**.

Fix — wipe the home volume, then rebuild:

```bash
# On the host, with the devcontainer stopped:
docker volume rm mee-pdn-home
```

Then rebuild the devcontainer. The volume is recreated from the fresh image, so the new tools land. You will lose container-local user state (shell history, Codex device login, anything cached only inside `/home/vscode`); configured Sandcat tokens re-authenticate on the next start.
