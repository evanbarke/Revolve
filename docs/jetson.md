# Jetson: discovery, native deployment, measurements

## Status (2026-09-29)

**Device validation performed** on the owner's Jetson through the
pre-configured SSH alias `jetson` (key authentication, non-interactive). Nothing
on the device was installed, upgraded, flashed or reconfigured; all work lives
under `~/revolve/` in the user's home directory.

| item | result |
| --- | --- |
| board | NVIDIA Jetson Orin Nano Engineering Reference Developer Kit Super |
| software | Ubuntu 22.04.5 LTS, L4T R36.4.7 (`nvidia-l4t-core 36.4.7`), kernel 5.15.148-tegra, glibc 2.35 |
| CPU / RAM / disk | 6 cores (ARMv8), 7.4 GiB RAM (+3.7 GiB swap), NVMe with 283 GB free |
| toolchain | rustc/cargo 1.95.0 in `~/.cargo/bin` (not on the non-interactive PATH), gcc 11.4 |
| power mode (read only) | `MAXN_SUPER` (reported by `nvpmodel -q`; not changed) |
| NVIDIA stack | CUDA runtime 12.6 packages and libnvinfer 10.3 present; `nvcc` not on PATH; `nvidia-jetpack` meta-package not installed. Not used: the core binary has no GPU dependency. |

Sanitised report: `reports/jetson-device-report.txt` (GPU UUID redacted; no
hostname, serials, addresses or environment).

## Native build

```
# on the device (source copied with `git archive HEAD | ssh jetson "tar -x -C ~/revolve/src"`)
cd ~/revolve/src
CARGO_TARGET_DIR=~/revolve/target ~/.cargo/bin/cargo build --release --locked -j 4
```

- First build (all dependencies, bundled SQLite, rustls): **156 s** with `-j 4`.
  Incremental rebuild after source changes: ~51 s.
- Output: `ELF 64-bit LSB pie executable, ARM aarch64`, 8.6 MB, dynamically
  linked against glibc; SQLite is compiled in (`rusqlite` `bundled`).
- `-j 4` leaves two cores and headroom for the desktop session; memory was not
  a constraint for this build.
- Cross-compilation was not needed and was not attempted.

## Verification on the device

`cargo test --release --locked -j 4` on the Jetson: **all 39 tests pass**
(16 unit + 23 integration, final commit), including the real
subprocess crash tests (the executor process exits at the effect/result gap and
a new process recovers), the
`flock`-based owner lock, the claim/correction race, and every demo scenario
on both arms.

Measured by `scripts/jetson-measure.sh` at the final commit
(`reports/jetson-m4-measure.log`, 2026-09-29 14:38 UTC):

| measurement | value | source |
| --- | --- | --- |
| process start-up (`--version`, `capabilities`, `doctor`) | 3–5 ms mean of 5 | wall clock |
| nominal run (3 kits, 38 actions, fresh state, process spawn and evaluation included) | 953 ms mean, 900–1052 ms, 20 repeats | wall clock |
| crash-after-effect / crash-before-effect | completed, evaluator PASS, 2 executor processes, 1 simulated crash each | demo output |
| unknown-outcome-blocked / -restored | blocked safely (PASS) / completed after evidence restored (PASS) | demo output |
| planner-unavailable (connection lost after start; replan later needed) | completed, 1 restore-planner intervention, PASS | demo output |
| all 10 demonstrations run | evaluator PASS for each | demo output |
| held-out benchmark (31 scenarios × 2 arms) | 62/62 PASS, 57 s total | `results/jetson-held-out-scripted/` |
| executor peak RSS | 7.3 MB (baseline) – 8.8 MB (Revolve) | `/proc/self/status` VmHWM, per process |
| state on disk | 22 MB for 92 runs | `du` |
| board input power during the workload | mean 5.1 W, max 6.1 W (idle baseline sample ≈ 4.6 W) | `tegrastats` VDD_IN, 84 × 1 s samples |
| max junction temperature | 58.7 °C | `tegrastats` tj |

An earlier run at the M4 commit gave the same picture (925 ms mean nominal run,
59.2 °C max junction temperature).

Power numbers are whole-board input readings during a CPU-light workload in
`MAXN_SUPER`; they are not a per-process energy measurement.

## Browser access from another machine

The server refuses non-loopback binds. Use an SSH tunnel:

```
# on the Jetson
~/revolve/target/release/revolve --state-dir ~/revolve/state serve --bind 127.0.0.1:8765
# on your workstation
ssh -N -L 8080:127.0.0.1:8765 jetson      # then open http://127.0.0.1:8080/
```

Verified: a crash-after-effect run was created, crashed (exit 86), resumed and
completed with 3 verified kits entirely through a tunnel (local port 18080 →
device port 8765); a POST without the client header was refused (403).

## Disconnected operation (H4)

The executor is local to the device; a remote planner only proposes. When the
planner connection is lost, already validated work continues (preconditions are
still checked on fresh observations; permissions and budgets are unchanged);
when a new plan is needed the run pauses in `awaiting_planner` with the reason.
No automatic fallback changes authority, and a reply for an obsolete goal
revision is rejected after reconnection. Pending effects are never replayed
because a connection restarted: recovery goes through reconciliation.

## Optional user-level service

`deploy/revolve-ui.user.service` is a template for a **user** systemd unit
(no root). It was not installed. To use it:

```
mkdir -p ~/.config/systemd/user && cp deploy/revolve-ui.user.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user start revolve-ui
```

## Not done / needs explicit authorisation

- No JetPack/OS upgrade, flashing, clock or power-mode changes, driver or
  system package installation. None were needed.
- No local model inference: see `docs/roadmap.md` (local planning is a later,
  measured experiment; weights and runtime must match JetPack 6 / L4T 36.4).
- `tegrastats` must be available to the user for the power figures; it was.
