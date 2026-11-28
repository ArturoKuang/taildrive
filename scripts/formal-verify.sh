#!/usr/bin/env bash
# Unified Outer Formal Verification Harness for TailDrive (G0 -> G1 -> G2 -> G3).
# Runs:
#   G0: Pinned tla2tools.jar SHA-256 verification + static analysis (rustfmt, clippy, shellcheck, actionlint, gitleaks)
#   G1: Exhaustive TLA+/TLC model checking + mandatory negative canary kill proofs
#   G2: Rust property-based + stateful refinement + concurrent quota harnesses + Reader sync spec validator
#   G3: Anti-cheat guard + spec-to-code sync_commit drift check
# Emits reviewable artifacts: formal-report.md and formal-report.json.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="standard"
SINGLE_MODEL=""
SKIP_CODE=0
SKIP_STATIC=0
REPORT_DIR="${FORMAL_REPORT_DIR:-$ROOT/.formal-reports}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="$2"
      shift 2
      ;;
    --model)
      SINGLE_MODEL="$2"
      shift 2
      ;;
    --skip-code)
      SKIP_CODE=1
      shift
      ;;
    --skip-static)
      SKIP_STATIC=1
      shift
      ;;
    --report-dir)
      REPORT_DIR="$2"
      shift 2
      ;;
    -h|--help)
      cat <<'EOF'
Usage: ./scripts/formal-verify.sh [--mode standard|deep] [--model <Name>] [--skip-code] [--skip-static] [--report-dir DIR]
EOF
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 2
      ;;
  esac
done

if [[ "$MODE" != "standard" && "$MODE" != "deep" ]]; then
  echo "Invalid --mode: $MODE (expected standard or deep)" >&2
  exit 2
fi

mkdir -p "$REPORT_DIR"

# Resolve pinned Java & tla2tools.jar (supports macmini-ci runner env and local dev).
resolve_java() {
  if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/java" ]]; then
    echo "$JAVA_HOME/bin/java"
  elif [[ -x "/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home/bin/java" ]]; then
    echo "/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home/bin/java"
  elif [[ -x "/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home/bin/java" ]]; then
    echo "/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home/bin/java"
  elif command -v java >/dev/null 2>&1; then
    command -v java
  else
    echo ""
  fi
}

JAVA_BIN="$(resolve_java)"
TLA_JAR="${TLA2TOOLS_JAR:-$HOME/ci/tools/tla2tools.jar}"
TLC_WORKERS="${TLC_WORKERS:-2}"
META_BASE="${TLC_META_ROOT:-${RUNNER_TEMP:-/tmp}}"

if [[ -z "$JAVA_BIN" ]]; then
  echo "FAIL: Java runtime not found. Set JAVA_HOME or install openjdk@17." >&2
  exit 1
fi
if [[ ! -f "$TLA_JAR" ]]; then
  echo "FAIL: tla2tools.jar not found at $TLA_JAR. Set TLA2TOOLS_JAR." >&2
  exit 1
fi

EXPECTED_SHA="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["tla2tools_sha256"])' "$ROOT/formal/manifest.json")"
ACTUAL_SHA="$(shasum -a 256 "$TLA_JAR" | awk '{print $1}')"
if [[ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]]; then
  echo "FAIL: tla2tools.jar SHA-256 mismatch! expected=$EXPECTED_SHA actual=$ACTUAL_SHA ($TLA_JAR)" >&2
  exit 1
fi
echo "==> Toolchain verified: $TLA_JAR (sha256=${ACTUAL_SHA:0:12}...) with $JAVA_BIN"

SCRATCH_META="$(mktemp -d "${META_BASE%/}/tlc-taildrive-XXXXXX")"
cleanup() {
  rm -rf "$SCRATCH_META"
}
trap cleanup EXIT

OVERALL_FAIL=0

# G0: Static Analysis
G0_STATUS="skipped"
if [[ "$SKIP_STATIC" -eq 0 ]]; then
  if "$ROOT/scripts/static-analysis.sh"; then
    G0_STATUS="pass"
  else
    G0_STATUS="fail"
    OVERALL_FAIL=1
  fi
fi

# G1: TLA+ / TLC Model Checking + Mandatory Canary Kill Proofs
echo "==> G1: TLA+/TLC model checking (mode=$MODE, workers=$TLC_WORKERS)"
G1_RESULTS_JSON="$REPORT_DIR/.g1-results.json"
G1_STATUS="pass"
if ! python3 - "$ROOT" "$JAVA_BIN" "$TLA_JAR" "$TLC_WORKERS" "$SCRATCH_META" "$MODE" "$SINGLE_MODEL" "$G1_RESULTS_JSON" <<'PY'
import json
import pathlib
import re
import subprocess
import sys
import time

root = pathlib.Path(sys.argv[1])
java_bin = sys.argv[2]
tla_jar = sys.argv[3]
workers = sys.argv[4]
meta_root = pathlib.Path(sys.argv[5])
mode = sys.argv[6]
single_model = sys.argv[7]
out_path = pathlib.Path(sys.argv[8])

manifest = json.loads((root / "formal/manifest.json").read_text(encoding="utf-8"))
models = manifest["models"]
if single_model:
    models = [m for m in models if m["name"] == single_model]
    if not models:
        print(f"FAIL: unknown model {single_model!r}", file=sys.stderr)
        sys.exit(2)

results = []
failed = False

def run_tlc(spec_rel, cfg_rel, tag):
    spec_path = root / spec_rel
    cfg_path = root / cfg_rel
    meta_dir = meta_root / tag
    meta_dir.mkdir(parents=True, exist_ok=True)
    cmd = [
        java_bin,
        "-XX:+UseParallelGC",
        "-Xmx2g",
        "-cp",
        tla_jar,
        "tlc2.TLC",
        "-noGenerateSpecTE",
        "-workers",
        str(workers),
        "-cleanup",
        "-metadir",
        str(meta_dir),
        "-config",
        str(cfg_path),
        str(spec_path),
    ]
    t0 = time.monotonic()
    proc = subprocess.run(cmd, cwd=spec_path.parent, capture_output=True, text=True)
    elapsed = round(time.monotonic() - t0, 2)
    output = proc.stdout + "\n" + proc.stderr
    states_gen = None
    states_distinct = None
    diameter = None
    m = re.search(r"(\d+)\s+states generated,\s+(\d+)\s+distinct states found", output)
    if m:
        states_gen = int(m.group(1))
        states_distinct = int(m.group(2))
    md = re.search(r"depth of the complete state graph search is\s+(\d+)", output)
    if md:
        diameter = int(md.group(1))
    violated = None
    mv = re.search(r"Error:\s+(?:Invariant|Temporal property|Deadlock)\s+([A-Za-z0-9_]+)?\s*(?:is violated|reached)", output)
    if mv:
        violated = mv.group(1) or "Deadlock"
    return proc.returncode, elapsed, states_gen, states_distinct, diameter, violated, output

for model in models:
    name = model["name"]
    cfg_key = "deep_config" if mode == "deep" else "config"
    cfg_rel = model[cfg_key]
    spec_rel = model["spec"]
    rc, elapsed, gen, distinct, diam, violated, out = run_tlc(spec_rel, cfg_rel, f"{name}-{mode}")
    model_ok = rc == 0 and "Model checking completed. No error has been found." in out
    if model_ok:
        print(f"  [PASS] {name} ({mode}): {gen} generated, {distinct} distinct, depth={diam}, {elapsed}s")
    else:
        print(f"  [FAIL] {name} ({mode}): exit={rc}, violated={violated}, {elapsed}s", file=sys.stderr)
        print(out[-2500:], file=sys.stderr)
        failed = True

    # Canary check: MUST FAIL on expected invariant
    canary_rel = model["canary_config"]
    expected_inv = model["canary_expected_invariant"]
    crc, celapsed, cgen, cdistinct, _, cviolated, cout = run_tlc(spec_rel, canary_rel, f"{name}-canary")
    canary_killed = (crc != 0) and (expected_inv in cout)
    if canary_killed:
        print(f"  [KILL] {name} canary rejected as expected ({expected_inv}, {celapsed}s)")
    else:
        print(
            f"  [FAIL] {name} canary was NOT killed on {expected_inv!r} (exit={crc}, violated={cviolated})!",
            file=sys.stderr,
        )
        print(cout[-2000:], file=sys.stderr)
        failed = True

    results.append({
        "name": name,
        "mode": mode,
        "spec": spec_rel,
        "config": cfg_rel,
        "status": "pass" if model_ok else "fail",
        "states_generated": gen,
        "distinct_states": distinct,
        "diameter": diam,
        "elapsed_sec": elapsed,
        "violated": violated,
        "canary_config": canary_rel,
        "canary_expected_invariant": expected_inv,
        "canary_killed": canary_killed,
        "canary_violated": cviolated,
        "canary_elapsed_sec": celapsed,
        "covered_files": model.get("covered_files", []),
        "properties": model.get("properties", []),
        "bounds": model.get("bounds", {}),
    })

out_path.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
if failed:
    sys.exit(1)
PY
then
  G1_STATUS="fail"
  OVERALL_FAIL=1
fi

# G2: Code-Level Formal Conformance Harnesses & Spec Schema Validator
G2_STATUS="skipped"
G2_DETAIL="skipped via --skip-code"
if [[ "$SKIP_CODE" -eq 0 ]]; then
  echo "==> G2: Rust property, refinement & concurrent quota harnesses + Reader sync spec validation"
  if [[ "$MODE" == "deep" ]]; then
    export PROPTEST_CASES="${PROPTEST_CASES:-1000}"
    export FORMAL_CASES="${FORMAL_CASES:-1000}"
  else
    export PROPTEST_CASES="${PROPTEST_CASES:-256}"
    export FORMAL_CASES="${FORMAL_CASES:-200}"
  fi
  export FORMAL_SEED="${FORMAL_SEED:-1337}"
  echo "  Settings: PROPTEST_CASES=$PROPTEST_CASES FORMAL_CASES=$FORMAL_CASES FORMAL_SEED=$FORMAL_SEED"

  G2_FAIL=0
  CARGO_NOTE="cargo test PASS"
  if ! cargo test --workspace --no-fail-fast; then
    CARGO_NOTE="cargo test FAIL"
    G2_FAIL=1
  fi

  SPEC_PY=""
  if [[ -x "$HOME/ci/tools/spec-venv/bin/python" ]]; then
    SPEC_PY="$HOME/ci/tools/spec-venv/bin/python"
  elif python3 -c "import jsonschema, yaml, openapi_spec_validator" >/dev/null 2>&1; then
    SPEC_PY="python3"
  fi

  SPEC_NOTE="validate.py skipped locally"
  if [[ -n "$SPEC_PY" ]]; then
    if "$SPEC_PY" "$ROOT/docs/reader-sync/tools/validate.py" "$ROOT/docs/reader-sync/spec"; then
      SPEC_NOTE="validate.py PASS"
    else
      SPEC_NOTE="validate.py FAIL"
      G2_FAIL=1
    fi
  elif [[ "${CI:-}" == "true" ]]; then
    echo "FAIL: spec-venv python dependencies missing in CI for docs/reader-sync/tools/validate.py" >&2
    SPEC_NOTE="validate.py MISSING in CI"
    G2_FAIL=1
  else
    echo "  [WARN] jsonschema/openapi-spec-validator not installed locally; skipping validate.py"
  fi

  G2_DETAIL="${CARGO_NOTE} (PROPTEST_CASES=$PROPTEST_CASES, FORMAL_CASES=$FORMAL_CASES, seed=$FORMAL_SEED); ${SPEC_NOTE}"
  if [[ "$G2_FAIL" -eq 0 ]]; then
    G2_STATUS="pass"
  else
    G2_STATUS="fail"
    OVERALL_FAIL=1
  fi
fi

# G3: Anti-cheat guard + drift check
echo "==> G3: Anti-cheat guard & spec-to-code drift detection"
G3_STATUS="pass"
if ! "$ROOT/scripts/verify-harness-guard.sh"; then
  G3_STATUS="fail"
  OVERALL_FAIL=1
fi
DRIFT_JSON="$("$ROOT/scripts/check-formal-drift.sh" --json)"

# Generate formal-report.json and formal-report.md
python3 - "$ROOT" "$REPORT_DIR" "$MODE" "$G0_STATUS" "$G1_STATUS" "$G2_STATUS" "$G2_DETAIL" "$G3_STATUS" "$G1_RESULTS_JSON" "$DRIFT_JSON" "$ACTUAL_SHA" <<'PY'
import datetime
import json
import os
import pathlib
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
report_dir = pathlib.Path(sys.argv[2])
mode = sys.argv[3]
g0_status = sys.argv[4]
g1_status = sys.argv[5]
g2_status = sys.argv[6]
g2_detail = sys.argv[7]
g3_status = sys.argv[8]
g1_results = json.loads(pathlib.Path(sys.argv[9]).read_text(encoding="utf-8"))
drift = json.loads(sys.argv[10])
tla_sha = sys.argv[11]

head_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
short_sha = head_sha[:7]
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

report = {
    "repo": "ArturoKuang/taildrive",
    "generated_at": now,
    "commit": head_sha,
    "mode": mode,
    "tla2tools_sha256": tla_sha,
    "gates": {
        "G0_static_analysis": g0_status,
        "G1_tla_models": g1_status,
        "G2_code_harnesses": g2_status,
        "G2_detail": g2_detail,
        "G3_anti_cheat_guard": g3_status,
        "G3_drift": drift,
    },
    "models": g1_results,
}

json_path = report_dir / "formal-report.json"
md_path = report_dir / "formal-report.md"
json_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

lines = [
    f"# Formal Verification Report — `taildrive` (`{short_sha}`)",
    "",
    f"- **Mode:** `{mode}` | **Timestamp:** `{now}` | **TLC `tla2tools.jar`:** `{tla_sha[:12]}...`",
    f"- **G0 Static Analysis & Code Quality Ratchet:** `{g0_status}`",
    f"- **G1 TLA+ Model Checking & Canaries:** `{g1_status}`",
    f"- **G2 Code Conformance Harnesses:** `{g2_status}` ({g2_detail})",
    f"- **G3 Anti-Cheat Guard:** `{g3_status}` | **Spec Drift vs `sync_commit` (`{drift['sync_commit'][:7]}`):** `{drift['status']}`",
    "",
    "## TLA+ Models & Negative Canary Proofs",
    "",
    "| Model | Status | States Generated | Distinct States | Depth | Time | Canary Killed (`Expected Invariant`) |",
    "| --- | --- | ---: | ---: | ---: | ---: | --- |",
]
for r in g1_results:
    canary_cell = f"YES (`{r['canary_expected_invariant']}`)" if r["canary_killed"] else f"NO (`{r['canary_expected_invariant']}`)"
    lines.append(
        f"| `{r['name']}` | **{r['status'].upper()}** | {r['states_generated'] or '-'} | {r['distinct_states'] or '-'} | {r['diameter'] or '-'} | {r['elapsed_sec']}s | {canary_cell} |"
    )

if drift.get("drifted_models"):
    lines += ["", "## Spec-to-Code Drift Advisory", ""]
    for item in drift["drifted_models"]:
        lines.append(f"- `{item['model']}` covered files changed since `{drift['sync_commit'][:7]}`: {', '.join('`' + f + '`' for f in item['touched_files'])}")

lines += [
    "",
    "## Verified Properties",
    "",
]
for r in g1_results:
    lines.append(f"### `{r['name']}`")
    for prop in r.get("properties", []):
        if isinstance(prop, dict):
            lines.append(f"- **`{prop.get('name')}`**: {prop.get('summary')} *(Harm prevented: {prop.get('harm')})*")
        else:
            lines.append(f"- `{prop}`")
    lines.append("")

md_text = "\n".join(lines).rstrip() + "\n"
md_path.write_text(md_text, encoding="utf-8")
summary_file = os.environ.get("GITHUB_STEP_SUMMARY")
if summary_file:
    with open(summary_file, "a", encoding="utf-8") as fh:
        fh.write(md_text)

print(f"==> Reports written to {md_path} and {json_path}")
PY
rm -f "$G1_RESULTS_JSON"
if [[ "$OVERALL_FAIL" -ne 0 ]]; then
  echo "==> TailDrive formal verification FAILED (see $REPORT_DIR/formal-report.md)." >&2
  exit 1
fi
echo "==> TailDrive formal verification PASSED."
