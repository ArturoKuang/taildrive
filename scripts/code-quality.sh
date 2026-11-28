#!/usr/bin/env bash
# Code Quality & Architectural Fitness Ratchet Gate for TailDrive.
# Enforces hard architectural boundaries and prevents regression of non-test panic calls,
# lint suppressions, and monolithic file growth beyond .ci/quality-ratchet.json ceilings.
# Usage:
#   ./scripts/code-quality.sh                # verify against .ci/quality-ratchet.json
#   ./scripts/code-quality.sh --update       # recompute ratchet ceilings (only tighten or intentionally update)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

UPDATE=0
if [[ "${1:-}" == "--update" ]]; then
  UPDATE=1
fi

python3 - "$ROOT" "$UPDATE" <<'PY'
import json
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
update = sys.argv[2] == "1"
ratchet_path = root / ".ci/quality-ratchet.json"

def strip_rust_test_module(src: str) -> str:
    # Strip trailing #[cfg(test)] mod tests { ... } blocks so production metrics ignore unit tests.
    m = re.search(r"^\s*#\[cfg\(test\)\]", src, re.M)
    return src[:m.start()] if m else src

def strip_rust_comments(src: str) -> str:
    src = re.sub(r"/\*.*?\*/", "", src, flags=re.S)
    src = re.sub(r"//[^\n]*", "", src)
    return src

boundary_errors = []

# Boundary 1: taildrive-core Cargo.toml must never depend on server/DB/HTTP-server crates.
core_toml = (root / "crates/taildrive-core/Cargo.toml").read_text(encoding="utf-8")
for forbidden in ("axum", "rusqlite", "opendal", "taildrive-server"):
    if re.search(rf"^\s*{re.escape(forbidden)}\b", core_toml, re.M):
        boundary_errors.append(f"architectural boundary violation: taildrive-core depends on {forbidden}")

# Boundary 2: reader sync module must remain isolated from TailDrive's taildrive.db (crate::db).
reader_dir = root / "crates/taildrive-server/src/reader"
for rs_file in sorted(reader_dir.glob("*.rs")):
    if rs_file.name == "tests.rs":
        continue
    prod = strip_rust_comments(strip_rust_test_module(rs_file.read_text(encoding="utf-8")))
    if re.search(r"\bcrate::db\b|\btaildrive\.db\b", prod):
        boundary_errors.append(
            f"architectural boundary violation: {rs_file.relative_to(root)} references taildrive.db / crate::db"
        )

# Boundary 3: zero unsafe code in workspace Rust source files.
prod_files = sorted(
    p for p in (root / "crates").glob("*/src/**/*.rs")
    if p.name != "tests.rs"
)

metrics = {
    "rust_prod_unwrap_calls": 0,
    "rust_prod_panic_macros": 0,
    "rust_prod_allow_attrs": 0,
    "rust_prod_files_over_600_loc": 0,
    "rust_prod_max_file_loc": 0,
    "ts_prod_any_or_ignore": 0,
}

for rs_file in prod_files:
    raw = rs_file.read_text(encoding="utf-8")
    prod_raw = strip_rust_test_module(raw)
    prod = strip_rust_comments(prod_raw)
    if re.search(r"\bunsafe\s*\{|\bunsafe\s+fn\b|\bunsafe\s+impl\b", prod):
        boundary_errors.append(f"memory safety violation: unsafe code in {rs_file.relative_to(root)}")
    loc = len(prod_raw.splitlines())
    metrics["rust_prod_max_file_loc"] = max(metrics["rust_prod_max_file_loc"], loc)
    if loc > 600:
        metrics["rust_prod_files_over_600_loc"] += 1
    metrics["rust_prod_unwrap_calls"] += len(re.findall(r"\.unwrap\s*\(", prod))
    metrics["rust_prod_panic_macros"] += len(re.findall(r"\b(?:panic|todo|unimplemented)!\s*\(", prod))
    metrics["rust_prod_allow_attrs"] += len(re.findall(r"#\[allow\(", prod))

web_src = root / "web/src"
if web_src.is_dir():
    for ts_file in sorted(list(web_src.glob("**/*.ts")) + list(web_src.glob("**/*.tsx"))):
        if ".test." in ts_file.name:
            continue
        text = ts_file.read_text(encoding="utf-8")
        metrics["ts_prod_any_or_ignore"] += len(
            re.findall(r":\s*any\b|\bas\s+any\b|@ts-ignore|@ts-expect-error|eslint-disable", text)
        )

if update:
    ratchet_path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "description": "Code quality ceilings for TailDrive. Values may tighten over time but CI blocks regressions.",
        "ceilings": metrics,
    }
    ratchet_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(f"Updated {ratchet_path.relative_to(root)}: {json.dumps(metrics)}")
    sys.exit(0)

if not ratchet_path.is_file():
    print(f"FAIL: missing {ratchet_path.relative_to(root)} (run with --update to initialize)", file=sys.stderr)
    sys.exit(1)

ceilings = json.loads(ratchet_path.read_text(encoding="utf-8"))["ceilings"]
ratchet_errors = []
improvements = []
for key, ceiling in ceilings.items():
    actual = metrics.get(key, 0)
    if actual > ceiling:
        ratchet_errors.append(f"{key}: actual={actual} exceeds ceiling={ceiling}")
    elif actual < ceiling:
        improvements.append(f"{key}: improved {ceiling} -> {actual} (tighten with ./scripts/code-quality.sh --update)")

if boundary_errors or ratchet_errors:
    print("Code quality & architectural fitness gate FAILED:", file=sys.stderr)
    for err in boundary_errors + ratchet_errors:
        print(f"  - {err}", file=sys.stderr)
    sys.exit(1)

print("  [PASS] architectural boundaries (core isolation, reader.db isolation, zero unsafe)")
print(f"  [PASS] quality ratchet ceilings ({', '.join(f'{k}={v}/{ceilings[k]}' for k, v in metrics.items())})")
for imp in improvements:
    print(f"  [TIP]  {imp}")
PY
