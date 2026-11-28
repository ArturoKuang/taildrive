#!/usr/bin/env bash
# Installs TailDrive's repository-managed git hooks (.githooks/pre-commit and .githooks/pre-push).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

chmod +x .githooks/pre-commit .githooks/pre-push \
  scripts/static-analysis.sh scripts/verify-harness-guard.sh \
  scripts/check-formal-drift.sh scripts/formal-verify.sh

git config core.hooksPath .githooks
echo "Installed git hooks: core.hooksPath=$(git config --get core.hooksPath)"

for tool in cargo rustfmt shellcheck actionlint gitleaks java; do
  if command -v "$tool" >/dev/null 2>&1; then
    printf '  [OK]      %s -> %s\n' "$tool" "$(command -v "$tool")"
  else
    printf '  [MISSING] %s\n' "$tool"
  fi
done
if [[ -f "${TLA2TOOLS_JAR:-$HOME/ci/tools/tla2tools.jar}" ]]; then
  printf '  [OK]      tla2tools.jar -> %s\n' "${TLA2TOOLS_JAR:-$HOME/ci/tools/tla2tools.jar}"
else
  printf '  [MISSING] tla2tools.jar (expected at %s)\n' "${TLA2TOOLS_JAR:-$HOME/ci/tools/tla2tools.jar}"
fi
