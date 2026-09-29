#!/usr/bin/env bash
# Bounded, repeatable measurements of the Revolve runtime on an edge device.
#
#   bash scripts/jetson-measure.sh <revolve-binary> <source-dir> <output-dir> [repeats]
#
# Runs only project-local work (fresh state under <output-dir>). Samples
# tegrastats while the workload runs, if available, and stops only the sampler
# it started. Changes no clocks, power modes, packages or services.
set -u
BIN="${1:?revolve binary}"; SRC="${2:?source dir}"; OUT="${3:?output dir}"; N="${4:-20}"
mkdir -p "$OUT"
STATE="$OUT/state"; rm -rf "$STATE"; mkdir -p "$STATE"
LOG="$OUT/measure.log"; : >"$LOG"
cd "$SRC" || exit 1
ms() { date +%s%3N; }
say() { printf '%s\n' "$*" | tee -a "$LOG"; }

say "# Revolve edge measurements"
say "utc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "binary: $("$BIN" --version) ($(stat -c %s "$BIN") bytes)"
say "arch: $(uname -m); kernel $(uname -r)"

SAMPLER=""
if command -v tegrastats >/dev/null 2>&1; then
  tegrastats --interval 1000 >"$OUT/tegrastats.log" 2>/dev/null &
  SAMPLER=$!
  say "tegrastats: sampling every 1000 ms (pid $SAMPLER, started by this script)"
fi
trap '[ -n "$SAMPLER" ] && kill "$SAMPLER" 2>/dev/null' EXIT

say ""; say "## Startup (process start to exit)"
for cmd in "--version" "capabilities" "doctor --state-dir $STATE"; do
  t=0; for i in 1 2 3 4 5; do s=$(ms); "$BIN" $cmd >/dev/null 2>&1; t=$((t + $(ms) - s)); done
  say "- revolve $cmd: mean $((t / 5)) ms over 5"
done

say ""; say "## Demonstrations (fresh run each; scripted planner and scripted human)"
for sc in nominal crash-after-effect crash-before-effect timeout-unknown unknown-outcome-blocked unknown-outcome-restored planner-unavailable goal-change unauthorized-proposal cancel; do
  s=$(ms)
  out="$("$BIN" --state-dir "$STATE" demo --scenario "$sc" --quiet 2>&1)"
  dt=$(( $(ms) - s ))
  status=$(printf '%s\n' "$out" | sed -n 's/^status: \([a-z_]*\).*/\1/p' | head -n1)
  verdict=$(printf '%s\n' "$out" | sed -n 's/.*=> \(PASS\|FAIL\)/\1/p' | head -n1)
  sup=$(printf '%s\n' "$out" | sed -n 's/^supervisor: //p' | head -n1)
  say "- $sc: $status, evaluator $verdict, ${dt} ms wall; $sup"
done

say ""; say "## Repeated workload: $N x nominal (3 kits, 38 actions each)"
min=999999; max=0; tot=0
for i in $(seq 1 "$N"); do
  s=$(ms); "$BIN" --state-dir "$STATE" demo --scenario nominal --quiet >/dev/null 2>&1; dt=$(( $(ms) - s ))
  tot=$((tot + dt)); [ $dt -lt $min ] && min=$dt; [ $dt -gt $max ] && max=$dt
done
say "- wall per run: mean $((tot / N)) ms, min $min ms, max $max ms"

say ""; say "## Held-out benchmark on device (both arms, scripted)"
s=$(ms)
"$BIN" --state-dir "$STATE" benchmark --suite held-out --out "$OUT/benchmark-held-out" >/dev/null 2>"$OUT/benchmark.stderr"
say "- benchmark wall: $(( $(ms) - s )) ms; report: $OUT/benchmark-held-out/report.md"
grep -E '^\| (success|recovery success|planner/model requests|mean wall|max peak RSS|false success|trials with incorrect)' "$OUT/benchmark-held-out/report.md" | tee -a "$LOG"

say ""; say "## State on disk"
say "- state directory size: $(du -sh "$STATE" | cut -f1) for $(ls "$STATE/runs" | wc -l) runs"

if [ -n "$SAMPLER" ]; then
  kill "$SAMPLER" 2>/dev/null; wait "$SAMPLER" 2>/dev/null; SAMPLER=""
  say ""; say "## tegrastats summary (source: tegrastats, 1 s samples during this script)"
  awk '
    { for (i = 1; i <= NF; i++) {
        if ($i == "VDD_IN") { split($(i+1), a, "mW"); p = a[1] + 0; sum += p; n++; if (p > pmax) pmax = p }
        if ($i ~ /^tj@/) { t = substr($i, 4) + 0; if (t > tmax) tmax = t }
        if ($i == "RAM") { split($(i+1), r, "/"); u = r[1] + 0; if (u > rmax) rmax = u }
    } }
    END { if (n) printf "- samples: %d; VDD_IN mean %.0f mW, max %d mW; max tj %.1f C; max system RAM used %d MB\n", n, sum / n, pmax, tmax, rmax }
  ' "$OUT/tegrastats.log" | tee -a "$LOG"
fi
say ""; say "done"
