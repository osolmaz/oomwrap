# oomwrap implementation plan

## Goal

Build a small Rust CLI that runs one local command under process-scoped RAM and
swap protection. It must protect interactive machines from large builds,
renderers, data jobs, model loads, inference servers, and similar workloads.

The primitive is:

```bash
oomwrap run [options] -- <command> [args...]
```

## Non-goals

- Do not replace `earlyoom` or another machine-wide safety mechanism.
- Do not install a persistent service by default.
- Do not delete caches, images, model weights, build outputs, or user files.
- Do not tune the child workload for performance or quality.
- Do not claim that a supervised command cannot trigger a driver or kernel fault.

## Core design

0.2.0 replaced the design of 0.1.0. [cgroup-guard-plan.md](cgroup-guard-plan.md)
holds the full design, the phases that built it, and the measurements that set
its defaults. In short, `oomwrap run`:

- finds the memory domain: the machine, or the smallest limited cgroup above the
  caller, and reads its size, free room and pressure (PSI);
- checks in a preflight that the domain has room for the job;
- starts the job through `oomwrap exec-job`, which sets `oom_score_adj` to
  `1000` and reports to the guard over a control socket;
- with the `cgroup` backend, runs the job in a transient scope of the systemd
  manager (`systemd-run --scope`) with `memory.max` and `memory.high`, and stops
  it through `cgroup.kill`, so a child that left the process group ends too;
- with the `poll` backend, where the `cgroup` backend cannot run, guards the
  process group as 0.1.0 did, with the new policy;
- stops the job on `job_limit`, `outside_oom_kill`, `job_pressure`,
  `domain_floor` or `domain_pressure`, with `SIGTERM` first and a kill after
  `--term-grace`;
- writes JSON Lines events by default, and with `--record-readings` every
  reading and decision as a trace;
- returns `137` when it stops a job.

A `generic` profile supports any command. Inference profiles add command
detection and require a machine-wide guard by default, because large local model
loads can also create memory pressure that the job's cgroup does not count.

## CLI surface

```bash
oomwrap doctor [--profile <name>] [--json]
oomwrap inspect [--json]
oomwrap run [options] -- <command> [args...]
oomwrap install-shims [options]
oomwrap uninstall-shims [options]
oomwrap wrap <path>
oomwrap unwrap <path>
```

Important run options:

```text
--profile auto|vllm|llama-cpp|sglang|trtllm|tgi|generic
--backend auto|cgroup|poll
--memory-max 85%
--memory-high 90%
--memory-swap-max 0
--min-mem 15%
--domain-pressure 10%      --domain-pressure-for 10s
--job-pressure 60%         --job-pressure-for 30s
--poll 1s
--term-grace 10s
--require-machine-guard | --allow-no-machine-guard
--event-log PATH|none
--log-command
--record-readings PATH
```

The policy file (`/etc/oomwrap.toml`, then
`${XDG_CONFIG_HOME:-~/.config}/oomwrap/config.toml`) sets the same values for
each profile. Shims and wrappers carry no policy.

## Adoption paths

General commands call `oomwrap run` directly.

Inference runtimes can also use PATH shims for known executable names or an
in-place wrapper for a fixed executable path. The original executable is kept as
a sidecar and `oomwrap unwrap` restores it exactly.

No shim or wrapper is installed without an explicit command.

## Safety boundaries

The guard rejects a launch when the memory domain has no room for it (exit 4).
High-risk inference profiles reject a launch when a machine-wide guard is
required but none runs (exit 3). An explicit job limit that the `cgroup` backend
cannot give also exits 3, so a job never runs without the limit that it asked
for. The `generic` profile lets the operator select the needed machine-wide
policy explicitly.

A guard error after the child starts must clean up the complete job.
Terminal foreground control must return to the caller after normal exit,
pressure stop, interruption, or internal error.

## Testing

Unit tests cover the parsers, the memory domain, the policy, event
serialization, shim and wrapper generation, and the handshake. They also replay
recorded traces from `tests/fixtures/traces` through the policy, so a change of
a default is checked against real runs.

Integration tests (`tests/cli.rs`) run the binary against fake cgroup trees,
fake `/proc` files and a fake `systemd-run`, with bounded child processes. They
cover exits, signals, preflight refusals, every stop reason, cleanup, the policy
file, `doctor`, shims and wrappers.

The real-system suite (`tests/real_system.rs`) runs only on throwaway CI
runners. There it lets small jobs run out of memory on purpose, each inside its
own small scope, and checks the kernel's limits and the stops. Only the
`Pressure traces` workflow, started by hand, puts a whole runner under memory
pressure, because the runner is discarded after the run. No test induces an
out-of-memory condition on a developer machine or brings a shared machine under
memory pressure.

## Packaging

oomwrap installs from crates.io with `cargo install oomwrap --locked`. From
0.2.0 on, each GitHub release also carries static musl binaries for `x86_64` and
`aarch64`, tested before the release, with SHA-256 checksums and build
provenance attestations. [RELEASING.md](RELEASING.md) describes the release.
