#!/usr/bin/env bash
# Builds the Suwayomi desktop shell (suwayomi / suwayomi.exe) with a real version
# number injected into tauri.conf.json — on Windows that lands in the PE version
# resource, elsewhere it is just the app metadata. Cross-platform.
#
# Version comes from the caller (CI computes it from this repo's commit count) via
# SUWAYOMI_TRAY_VERSION, and must be 3-segment semver: tauri-build rejects anything
# else, and on Windows the FileVersion/ProductVersion strings show the same digits.
# Without it we fall back to this repo's own commit count, using the rule in
# .github/workflows/release.yml.
#
# Usage: bash build-tray.sh  (from anywhere; runs cargo build --release)

set -euo pipefail
cd "$(dirname "$0")"

# --- version: caller-supplied semver, else this repo's commit count ---
VER="${SUWAYOMI_TRAY_VERSION:-}"
if [ -z "$VER" ]; then
  COUNT="$(git rev-list --count HEAD 2>/dev/null || echo 0)"
  VCODE=$((COUNT + 1000))
  VER="1.$((COUNT/100)).$(printf '%02d' $((COUNT%100)))"
  echo "[build-tray] versionCode=${VCODE} -> version ${VER} (commit-count fallback)"
else
  echo "[build-tray] version=${VER}"
fi

# --- inject version into tauri.conf.json, restore afterwards (even on failure) ---
CONF="tauri.conf.json"
cp "$CONF" "$CONF.bak"
restore() {
  mv "$CONF.bak" "$CONF"
  echo "[build-tray] tauri.conf.json restored"
}
trap restore EXIT

# Linux runner 只有 python3，Windows runner 是 python——两者都兼容
PY="python3"
command -v "$PY" >/dev/null 2>&1 || PY="python"
"$PY" - "$VER" <<'PYEOF'
import json, sys
ver = sys.argv[1]
p = "tauri.conf.json"
d = json.load(open(p, encoding="utf-8"))
d["version"] = ver
with open(p, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, ensure_ascii=False)
    f.write("\n")
print(f"[build-tray] version injected: {ver}")
PYEOF

# --- build ---
cargo build --release
# 产物名：Windows 带 .exe，Linux/macOS 无后缀
OUT="target/release/suwayomi"
[ -f "${OUT}.exe" ] && OUT="${OUT}.exe"
echo "[build-tray] done: ${OUT}"
