# Token Setup for Devcontainer

One-time setup on the host machine to work with devcontainers.

## Required Tokens

1. **GitHub Personal Access Token** — for cloning private repositories, push/pull, access to GitHub Packages
2. **Claude Code OAuth Token** — for using Claude Code with your Pro/Max subscription (no API billing)

---

## 1. Obtaining GitHub Personal Access Token (classic)

### Steps:

1. Open https://github.com/settings/tokens/new
2. Fill in **Note** — descriptive name, e.g., `sandcat-devcontainer` or `mee-pdn-devcontainer`
3. Set **Expiration** — recommended: 90 days
4. Select scopes:
   - ✅ **repo** — full access to private repositories (clone, push, pull)
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

## 3. Creating Configuration File

### Where tokens are stored:

Tokens are stored in `~/.config/sandcat/settings.json` on the **host machine**.

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
    {"action": "allow", "host": "*.claude.com"}
  ]
}
SETTINGS
```

## Additional Information

### Security

- Tokens are stored only on the host machine in `~/.config/sandcat/settings.json`
- Inside the container, tokens are available as environment variables
- Mitmproxy intercepts HTTP(S) requests and replaces placeholders with real tokens
- Tokens **are not logged** and **not saved in command history**

### Token Expiration

- **GitHub PAT (classic)**: expiration set by you (recommended 90 days)
- **Claude OAuth**: valid for 1 year, after which you need to re-run `claude setup-token`

### Replacing Tokens

If a token expires or is compromised:

1. Generate a new token (following instructions above)
2. Update the value in `~/.config/sandcat/settings.json`
3. Restart devcontainer: **Dev Containers: Rebuild Container**

---

## Where Tokens Are Used

### GITHUB_TOKEN

- `git clone` private repositories
- `git push` / `git pull`
- `gh` CLI (GitHub CLI)
- Access to GitHub Packages / Container Registry

### CLAUDE_CODE_OAUTH_TOKEN

- `claude` command inside devcontainer
- Claude Code agent (AI assistant)
- Uses your Pro/Max subscription instead of API billing

### GIT_USER_NAME / GIT_USER_EMAIL

- Git commit author
- Automatically configured on container startup
