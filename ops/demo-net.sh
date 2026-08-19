#!/bin/sh
# Reaching the demo's nodes from a caller that is not the daemon's host: a
# published port belongs to that host, so a run from another container on
# the same daemon drives the nodes' own addresses on their network instead —
# which it can only do from a namespace attached to that network.
#
#   join <container>   attaches that container's namespace to the network
#   leave <container>  detaches it again, before the nodes come down: a
#                      network with a member left on it outlives the run
#   url <service>      the base URL one node answers on that network
set -eu

# A node's container, by the service name the compose file gives it.
node() {
  cid=$(docker compose -f ops/compose.yml ps -q "$1")
  [ -n "$cid" ] || { echo "no container for $1" >&2; exit 1; }
  echo "$cid"
}

# The network the nodes are on, asked of a node rather than spelled here:
# compose names it after the project, and one node sits on one network.
network() {
  first=$(docker compose -f ops/compose.yml config --services | head -1)
  net=$(docker inspect -f '{{ range $name, $_ := .NetworkSettings.Networks }}{{ $name }}{{ end }}' "$(node "$first")")
  [ -n "$net" ] || { echo "the nodes are on no network" >&2; exit 1; }
  echo "$net"
}

case "${1:-}" in
  join) docker network connect "$(network)" "$2" ;;
  leave) docker network disconnect "$(network)" "$2" ;;
  url)
    ip=$(docker inspect -f '{{ range .NetworkSettings.Networks }}{{ .IPAddress }}{{ end }}' "$(node "$2")")
    [ -n "$ip" ] || { echo "$2 has no address on the demo network" >&2; exit 1; }
    # The port the image serves on: nothing is published here, so what a
    # caller on this network reaches is the port itself.
    printf 'http://%s:3011\n' "$ip"
    ;;
  *) echo "usage: demo-net.sh join|leave <container> | url <service>" >&2; exit 1 ;;
esac
