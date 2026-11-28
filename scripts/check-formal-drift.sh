#!/usr/bin/env bash
# Checks whether source files covered by formal/manifest.json changed since sync_commit
# (or in staged git changes when --staged is passed).
# Emits human-readable status or JSON (--json). Exits 0 by default (advisory); pass --strict to fail on drift.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

STAGED=0
JSON_OUT=0
STRICT=0
for arg in "$@"; do
  case "$arg" in
    --staged) STAGED=1 ;;
    --json) JSON_OUT=1 ;;
    --strict) STRICT=1 ;;
  esac
done

python3 - "$ROOT" "$STAGED" "$JSON_OUT" "$STRICT" <<'PY'
import json
import pathlib
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
staged = sys.argv[2] == "1"
json_out = sys.argv[3] == "1"
strict = sys.argv[4] == "1"

manifest = json.loads((root / "formal/manifest.json").read_text(encoding="utf-8"))
sync_commit = manifest["sync_commit"]

def git_lines(args):
    proc = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    if proc.returncode != 0:
        return []
    return [line.strip() for line in proc.stdout.splitlines() if line.strip()]

if staged:
    changed = set(git_lines(["diff", "--cached", "--name-only"]))
else:
    committed = set(git_lines(["diff", "--name-only", f"{sync_commit}..HEAD"]))
    working = set(git_lines(["diff", "--name-only"]))
    staged_now = set(git_lines(["diff", "--cached", "--name-only"]))
    changed = committed | working | staged_now

report = {
    "sync_commit": sync_commit,
    "mode": "staged" if staged else "since_sync_commit",
    "status": "in-sync",
    "drifted_models": [],
}

for model in manifest.get("models", []):
    covered = model.get("covered_files", [])
    touched = sorted(f for f in covered if f in changed)
    if touched:
        report["status"] = "drift-detected"
        report["drifted_models"].append({
            "model": model["name"],
            "touched_files": touched,
        })

if json_out:
    print(json.dumps(report, indent=2))
else:
    if report["status"] == "in-sync":
        print(f"Formal spec drift check: IN SYNC (sync_commit={sync_commit[:7]}).")
    else:
        print(f"Formal spec drift check: DRIFT DETECTED since {sync_commit[:7]}:")
        for item in report["drifted_models"]:
            print(f"  - {item['model']}: {', '.join(item['touched_files'])}")
        print("  Action: audit formal models against covered code changes and update formal/manifest.json sync_commit.")

if strict and report["status"] != "in-sync":
    sys.exit(2)
PY
