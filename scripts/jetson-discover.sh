#!/usr/bin/env bash
# Read-only, idempotent environment discovery for Revolve edge deployment.
#
#   bash scripts/jetson-discover.sh [OUTPUT_FILE]    (default: ./device-report.txt)
#
# Safe on any Linux host: every probe tolerates missing files/commands. It does
# not change clocks, power modes, packages, services or files outside OUTPUT_FILE.
# It deliberately omits hostnames, serial numbers, MAC/IP addresses, usernames
# and environment variables so the report can be shared.
set -u
OUT="${1:-./device-report.txt}"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

have() { command -v "$1" >/dev/null 2>&1; }
section() { printf '\n## %s\n' "$1" >>"$TMP"; }
run() { # run "label" cmd... ; never fails the script
  local label="$1"; shift
  printf -- '- %s: ' "$label" >>"$TMP"
  if out="$("$@" 2>/dev/null)"; then
    printf '%s\n' "$(printf '%s' "$out" | head -n 20)" >>"$TMP"
  else
    printf 'unavailable\n' >>"$TMP"
  fi
}

printf '# Revolve device discovery report\n\ngenerated_utc: %s\nscript: scripts/jetson-discover.sh (read-only)\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$TMP"

section "Platform"
run "architecture" uname -m
run "kernel" uname -r
run "os" sh -c '. /etc/os-release && printf "%s" "$PRETTY_NAME"'
run "l4t release" sh -c 'head -n 1 /etc/nv_tegra_release'
run "board model" sh -c "tr -d '\\0' </proc/device-tree/model"
run "cpu cores" nproc
run "cpu model" sh -c "grep -m1 -E 'model name|Processor' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //'"

section "Memory and storage"
run "memory" sh -c 'free -h | sed -n "1,3p"'
run "home filesystem" sh -c 'df -h "$HOME" | tail -n 1 | awk "{print \$2\" total, \"\$4\" free, \"\$5\" used\"}"'
run "tmp filesystem" sh -c 'df -h /tmp | tail -n 1 | awk "{print \$2\" total, \"\$4\" free\"}"'

section "Toolchain (PATH, then ~/.cargo/bin)"
for t in rustc cargo; do
  if have "$t"; then run "$t" "$t" --version
  elif [ -x "$HOME/.cargo/bin/$t" ]; then run "$t (~/.cargo/bin)" "$HOME/.cargo/bin/$t" --version
  else printf -- '- %s: not found\n' "$t" >>"$TMP"; fi
done
run "cc" sh -c 'cc --version | head -n 1'
run "glibc" sh -c 'ldd --version | head -n 1'

section "NVIDIA software (package versions only; absence is not a failure)"
if have dpkg-query; then
  for p in nvidia-l4t-core nvidia-jetpack cuda-toolkit-12-6 cuda-runtime-12-6 libnvinfer10 tensorrt; do
    v="$(dpkg-query -W -f='${Version}' "$p" 2>/dev/null || true)"
    printf -- '- %s: %s\n' "$p" "${v:-not installed}" >>"$TMP"
  done
  run "cuda packages (first 5)" sh -c "dpkg-query -W -f='\${Package} \${Version}\n' 'cuda-*' 2>/dev/null | head -n 5"
else
  printf -- '- dpkg-query: not available\n' >>"$TMP"
fi
if have nvcc; then run "nvcc" sh -c 'nvcc --version | tail -n 1'; else printf -- '- nvcc: not on PATH
' >>"$TMP"; fi
# Device UUIDs are hardware identifiers: redact them.
if have nvidia-smi; then run "nvidia-smi" sh -c "nvidia-smi -L | sed -E 's/\(UUID: [^)]*\)/(UUID redacted)/'"; else printf -- '- nvidia-smi: not present (normal on Jetson; not evidence of a GPU problem)\n' >>"$TMP"; fi

section "Power and thermal (read-only; only if permitted)"
run "nvpmodel query" sh -c 'nvpmodel -q 2>/dev/null | head -n 3'
if have tegrastats; then
  # Bounded capture of a process this script starts; `timeout` stops only that child.
  run "tegrastats (3 samples)" sh -c 'timeout 4 tegrastats --interval 1000; [ -n "x" ]'
else
  printf -- '- tegrastats: not on PATH\n' >>"$TMP"
fi
run "thermal zones" sh -c 'for z in /sys/class/thermal/thermal_zone*; do [ -r "$z/type" ] && printf "%s=%s " "$(cat $z/type)" "$(cat $z/temp)"; done'

mkdir -p "$(dirname "$OUT")"
cp "$TMP" "$OUT"
printf 'wrote %s\n' "$OUT"
