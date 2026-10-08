# drowse

A sleep tracker for your laptop's SoC. drowse measures how well the CPU
*package* reaches deep idle, names what keeps it awake, and (M3) applies
reversible fixes — all agent-first: every query and action is a structured
JSON-RPC verb, and the CLI and TUI are thin clients of the same surface.

Near-idle battery life is governed by one thing: the fraction of time the
entire package (cores, fabric, memory controller, GPU) is asleep at once. That
fraction is shredded by frequent wakeups. powertop reports raw wakeups and
streams every kernel event to userspace (becoming a wakeup source itself);
drowse measures *package-sleep breaks*, attributes them in-kernel, and is built
to never perturb the idle it measures.

## Status

| Milestone | What | State |
|---|---|---|
| **M1** measure | Sleep-Efficiency scorecard, per-CPU C-state residency | ✅ done |
| **M2** attribute | eBPF wakeup blame chain, `offenders.get`, MCP adapter | ✅ done |
| **M3** act | tiered reversible power knobs + elastic-service caps | 🚧 in progress |

## Install / build

Needs Rust 1.98+ (edition 2024). The eBPF attribution additionally needs, at
build time, `clang` and `bpftool` (Arch: `pacman -S bpf`); a committed
`vmlinux.h` means no kernel headers are required. Loading eBPF needs a BTF
kernel and root (see below).

```
cargo build --release
```

Binaries: `drowsed` (the daemon) and `drowse` (the client).

## Use

```
# start the daemon (unprivileged: cpuidle/battery/gpu; add sudo for RAPL watts + attribution)
drowsed &

# how well is the package sleeping right now?
drowse check                 # SE 79/100 (B) · 8.2 W · 91% charge

# the full scorecard, per-CPU residency
drowse score --window 10
drowse score --json | jq .   # machine-readable; JSON is the default when piped

# what is keeping the CPU awake, ranked by battery cost? (needs root to load eBPF)
sudo drowsed & drowse offenders --top 10
#  1  proc:1044:syncthing   1341 pkg/s  2182 wake/s  27%
#  2  workqueue:kcryptd_crypt 301 pkg/s   788 wake/s  10%
```

Offenders are ranked by **package-sleep breaks per second** — the wakeups that
drag the *whole* SoC out of deep idle — not raw wakeup count. On the reference
machine this correctly surfaces `syncthing` as the #1 battery cost even though a
kernel timer has more raw wakeups.

### Driving it from an agent

```
drowse mcp-serve             # stdio MCP server exposing the read verbs as tools
```

Tools: `sleep_efficiency`, `wakeup_offenders`, `check_sources` (plan/apply/revert
land with M3). MCP protocol 2026-07-28, newline-framed JSON-RPC.

## How it stays cheap

drowse's own footprint is a tested budget: **≤ 1.1 wakeups per sampling tick**,
measured live in CI (`crates/drowsed/tests/self_footprint.rs`). An idle daemon
with no active session arms zero timers and blocks indefinitely. High-latency
reads that wake hardware — the battery EC gauges (~4.5 ms, ~13 wakeups each),
and firmware/SMU reads — run on a dedicated slow-lane thread so the fast
sampler never blocks. Attribution counts live in-kernel and are drained in one
batch on a slow cadence; nothing crosses to userspace per event.

## Architecture

```
crates/
  drowse-api/         wire contract: every object an agent sees, nothing else
  drowse-core/        Collector + Attributor + PolicyEngine seams; the sampling
                      Engine; a "server as a function" service stack (Tower)
  drowse-collectors/  cpuidle · battery · rapl · amdgpu  (pread on kept fds)
  drowse-ebpf/        CO-RE eBPF wakeup attribution (libbpf-rs)
  drowse-policy/      the act layer (M3): tiered knobs, journal, verify-after-apply
  drowsed/            the daemon: ppoll transport, JSON-RPC, sampler + lanes
  drowse/             the CLI + mcp-serve: thin clients of the daemon's verbs
```

Verbs are plain functions lifted into a `tower_service::Service`; filters are
`tower_layer::Layer`s; the daemon is a composed stack — "your server as a
function" (Eriksen 2013), without an async runtime (the no-wakeup promise rules
one out). The daemon runs as a privileged system instance (RAPL, eBPF) or an
unprivileged on-demand user instance; which socket answers *is* the privilege
model, and every source degrades to a typed reason rather than failing.

## Design

The full engineering design — metric definition, telemetry ground truth on AMD
Zen 4, the attribution and policy designs — is at
`https://claude.ai/code/artifact/d6562afb-e384-472a-a840-e72dc6d3f81f`.

## License

MIT OR Apache-2.0.
