#!/usr/bin/env bash
set -euo pipefail

# Migrate TailDrive Garage storage from internal SSD to an external SSD/DAS volume
# Usage:
#   ./deploy/switch-storage-root.sh --target /Volumes/MySSD/taildrive --quota-gb 500 [--dry-run]

TARGET=""
QUOTA_GB=""
DRY_RUN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --quota-gb) QUOTA_GB="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    *) echo "Unknown arg: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$TARGET" || -z "$QUOTA_GB" ]]; then
  echo "Usage: $0 --target /Volumes/<Drive>/taildrive --quota-gb <GB> [--dry-run]" >&2
  exit 1
fi

GARAGE_CFG="$HOME/.taildrive/garage/config.toml"
ENV_FILE="$HOME/.taildrive/taildrive.env"
QUOTA_BYTES=$(( QUOTA_GB * 1024 * 1024 * 1024 ))

echo "Target storage root: $TARGET"
echo "New quota: ${QUOTA_GB} GB (${QUOTA_BYTES} bytes)"

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[DRY RUN] Would stop com.local.taildrive-server & com.local.garage"
  echo "[DRY RUN] Would rsync ~/.taildrive/garage/{meta,data} -> ${TARGET}/garage/{meta,data}"
  echo "[DRY RUN] Would update $GARAGE_CFG and TAILDRIVE_QUOTA_BYTES in $ENV_FILE"
  echo "[DRY RUN] Would restart LaunchAgents and verify /v1/health"
  exit 0
fi

mkdir -p "$TARGET/garage/meta" "$TARGET/garage/data"

launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.local.taildrive-server.plist" 2>/dev/null || true
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.local.garage.plist" 2>/dev/null || true

rsync -aH "$HOME/.taildrive/garage/meta/" "$TARGET/garage/meta/"
rsync -aH "$HOME/.taildrive/garage/data/" "$TARGET/garage/data/"

python3 - "$GARAGE_CFG" "$ENV_FILE" "$TARGET" "$QUOTA_BYTES" <<'PY'
import pathlib, re, sys
gcfg, envf, target, qbytes = sys.argv[1:]
p = pathlib.Path(gcfg)
txt = p.read_text()
txt = re.sub(r'^metadata_dir\s*=.*$', f'metadata_dir = "{target}/garage/meta"', txt, flags=re.M)
txt = re.sub(r'^data_dir\s*=.*$', f'data_dir = "{target}/garage/data"', txt, flags=re.M)
p.write_text(txt)

ep = pathlib.Path(envf)
etxt = ep.read_text()
etxt = re.sub(r'^TAILDRIVE_QUOTA_BYTES=.*$', f'TAILDRIVE_QUOTA_BYTES={qbytes}', etxt, flags=re.M)
ep.write_text(etxt)
PY

launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.local.garage.plist"
sleep 2
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.local.taildrive-server.plist"
sleep 2

curl -sf -H "Tailscale-User-Login: verify@ts.net" http://127.0.0.1:8787/v1/health && echo ""
echo "Storage root migration complete. Update Garage node capacity via: garage -c $GARAGE_CFG layout assign -z dc1 -c ${QUOTA_GB}G <NODE_ID> && garage -c $GARAGE_CFG layout apply --version <NEXT>"
