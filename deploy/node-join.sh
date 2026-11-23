#!/usr/bin/env bash
set -euo pipefail

# Join a new Garage storage node over Tailscale into the TailDrive cluster.
# Run on macmini-ci after starting garage on the new node with the same rpc_secret.
# Usage: ./deploy/node-join.sh <NEW_NODE_ID@NEW_TAILSCALE_IP:3901> <ZONE> <CAPACITY_E_G_2T> <NEXT_LAYOUT_VERSION>

if [[ $# -ne 4 ]]; then
  echo "Usage: $0 <NEW_NODE_ID@TAILSCALE_IP:3901> <ZONE> <CAPACITY> <NEXT_LAYOUT_VERSION>" >&2
  exit 1
fi

PEER="$1"
ZONE="$2"
CAPACITY="$3"
VERSION="$4"
NODE_ID="${PEER%%@*}"
CFG="$HOME/.taildrive/garage/config.toml"

garage -c "$CFG" node connect "$PEER"
garage -c "$CFG" layout assign -z "$ZONE" -c "$CAPACITY" "$NODE_ID"
garage -c "$CFG" layout show
echo "Applying layout version $VERSION..."
garage -c "$CFG" layout apply --version "$VERSION"
garage -c "$CFG" status
