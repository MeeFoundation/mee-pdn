#!/bin/bash
#
# Entrypoint: this workspace's root-phase steps, then sandcat's own entrypoint
# (sandcat/scripts/app-init.sh), which drops to vscode and runs
# project-user-init.sh ahead of the container's command.
#
set -e

# Volumes mounted over workspace paths are created root-owned.
find /workspaces -mindepth 2 -maxdepth 3 -type d -name target -user root \
    -exec chown vscode:vscode {} + 2>/dev/null || true

exec /usr/local/bin/app-init.sh /usr/local/bin/project-user-init.sh "$@"
