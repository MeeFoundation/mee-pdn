#!/bin/bash
#
# Entrypoint: this workspace's root-phase steps, then sandcat's own entrypoint
# (sandcat/scripts/app-init.sh), which drops to vscode and runs
# project-user-init.sh ahead of the container's command.
#
set -e

# The socket's group differs per host; testcontainers needs vscode in it.
if [ -S /var/run/docker.sock ]; then
    DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
    DOCKER_GROUP=$(getent group "$DOCKER_GID" | cut -d: -f1)
    if [ -z "$DOCKER_GROUP" ]; then
        groupmod -g "$DOCKER_GID" docker 2>/dev/null || groupadd -g "$DOCKER_GID" docker
        DOCKER_GROUP=docker
    fi
    usermod -aG "$DOCKER_GROUP" vscode
    echo "vscode joined $DOCKER_GROUP (GID=$DOCKER_GID) for the Docker socket"
fi

# Volumes mounted over workspace paths are created root-owned.
find /workspaces -mindepth 2 -maxdepth 3 -type d -name target -user root \
    -exec chown vscode:vscode {} + 2>/dev/null || true

exec /usr/local/bin/app-init.sh /usr/local/bin/project-user-init.sh "$@"
