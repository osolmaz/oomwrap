---
name: memory-safe-launch
description: Use before launching a local command that can consume large RAM or swap, including builds, renderers, data jobs, model loads, and inference servers. Uses oomwrap for kernel memory limits, memory preflight, pressure stops, cleanup of the whole job, staged checks, and recovery.
---

# Memory-safe launch

Use this procedure before a local command can consume enough RAM or swap to
freeze the machine.

Canonical source:
<https://github.com/osolmaz/oomwrap/tree/main/.agents/skills/memory-safe-launch>

## 1. Confirm the target

Confirm that the requested work must run locally. If the user named a remote
endpoint, test that endpoint instead of starting a local fallback.

Do not wrap an existing agent, service, or unrelated process. oomwrap starts and
owns one new job, in its own cgroup scope when the machine supports it. It does
not adopt a process that is already running.

## 2. Check authority and provenance

Confirm that the user authorized the command, its inputs, its expected disk use,
and any required download or installation.

For inference runtimes, apply `$manage-runtimes` before downloading, installing,
building, patching, or replacing a runtime. A benchmark or serving request does
not authorize third-party executable code or a source build.

Do not delete caches, images, model files, build outputs, or other useful data to
pass a disk check without approval for the exact cleanup.

## 3. Check the machine

Run:

```bash
command -v oomwrap
oomwrap doctor
free -h
swapon --show
df -h .
ps -eo pid,ppid,stat,rss,etime,cmd --sort=-rss | head -25
```

For a GPU workload, also run `nvidia-smi` or the platform's normal device status
command.

On macOS, use the platform tools instead:

```bash
memory_pressure
sysctl vm.swapusage
vm_stat
top -l 1 -n 25 -o mem -stats pid,command,mem
```

Read the `doctor` report. It names the backend that oomwrap will use (`cgroup`
or `poll`) and why, the memory domain and its free room, the resolved limits with
the source of each, the machine-wide guards, and every repair it suggests, such
as `loginctl enable-linger` or an outdated shim. Use `oomwrap doctor --profile
<name>` for the profile of the launch, and `--json` for data.

Stop when an unrelated large process already makes the launch unsafe. Do not
terminate another agent or process without explicit approval.

On macOS, skip the oomwrap check and installation below. The `oomwrap` binary is
Linux-only, and macOS does not use `earlyoom`.

If oomwrap is absent, install it only when installation is authorized:

```bash
cargo install oomwrap --locked
```

Verify installation with `oomwrap --version` and `oomwrap inspect`. This skill
describes oomwrap 0.2.0. Version 0.1.0 had other flags (`--min-swap`,
`--require-earlyoom`); when `oomwrap --version` prints 0.1.0, ask before
upgrading it, because other jobs may use it.

## 4. Set memory limits

Estimate the workload's steady-state use and temporary peak. Keep enough RAM for
the operating system, terminal, browser, and other active work.

oomwrap derives its defaults from the memory domain, the memory that the job
competes for: the machine, or the cgroup of a container or limited service.

| Setting | Default |
| --- | --- |
| `--memory-max` (hard limit of the job) | the smallest of `85%`, the free room at launch minus `--min-mem`, and any lower limit above the job |
| `--memory-high` (soft limit) | `90%` of `--memory-max` |
| `--min-mem` (free room that must stay) | `15%`, at least `256M`, at most `16G` |
| `--job-pressure` | `60%` for `30s` |
| `--domain-pressure` | `10%` for `10s`, counted only below 25% free room |

Use the policy file (`/etc/oomwrap.toml`, then
`~/.config/oomwrap/config.toml`) or the repository's values when they exist;
`doctor` shows which file set each value. Otherwise the defaults fit most
launches. Give `--memory-max` when the workload's peak is known, so the kernel
bounds the job. An explicit limit needs the `cgroup` backend: without it,
oomwrap exits 3 and runs nothing.

Lower an established floor or raise a limit only when the user explicitly
approves the named command and values. An override must not disable the guard.
Keep machine-wide protection active for a high-risk launch.

Free swap is not a stop condition in 0.2.0. Do not pass `--min-swap`: it no
longer exists, and an unknown flag before the command is an error.

## 5. Launch through oomwrap

Use the generic profile for builds, rendering, conversion, extraction, and other
commands:

```bash
oomwrap run \
  --profile generic \
  --memory-max '<approved job limit>' \
  --event-log '<durable event-log path>' \
  -- <command> [args...]
```

Known inference-engine commands are detected automatically:

```bash
oomwrap run \
  --memory-max '<approved job limit>' \
  --event-log '<durable event-log path>' \
  -- llama-server --model '<model path>'
```

Leave out `--memory-max` to use the default. Without `--event-log`, events go
to `~/.local/state/oomwrap/`, one file per day.

On Linux, inference profiles require a machine-wide guard by default: a running
`earlyoom`, `systemd-oomd` that watches the job, or a guard that the policy file
names in `machine-guards`. Keep it as the safety net; oomwrap is the guard of
the job. Do not use `--allow-no-machine-guard` for a large model load.

On a machine where the GPU shares system memory, the cgroup does not count the
GPU's allocations, so `--memory-max` does not bound them. Keep the floor and the
domain pressure stop, and watch free memory during the load.

On macOS, oomwrap and earlyoom do not apply: oomwrap is Linux-only, and
earlyoom does not exist on macOS. Do not install either one there and do not
require a machine-wide watchdog. macOS compresses memory and applies its own
jetsam policy, so it does not freeze as hard as Linux. Use the macOS checks
from step 3, keep the staged load, and stop the process group that this launch
owns when pressure stays critical.

## 6. Increase load in stages

Start with the smallest action that proves the command works:

1. Start the guarded command.
2. Confirm readiness or a first bounded output.
3. Inspect memory, swap, logs, and the oomwrap event log.
4. Run one representative small input.
5. Run one representative peak input only after the small input is stable.
6. Start the full workload only after the peak check passes.

Do not run parallel peak tests. Stop when memory headroom declines without
recovery, swap approaches its floor, oomwrap stops the group, or the command
shows a deterministic shared defect.

## 7. Apply inference-specific checks

For a local inference server:

- record the full model ID and revision;
- record the runtime owner, version, commit, image digest, or package version;
- record context, concurrency, batching, cache, quantization, and speculative
  decoding settings;
- send one real request through the intended model;
- verify from runtime evidence that the requested kernels and backend executed;
- stop on fallback, emulation, unsupported-backend warnings, version mismatch,
  or an unexpected kernel.

A successful import or health check is not backend attestation. If the benchmark
needs long context, run one guarded long-prefill check before benchmark traffic.

Inference runtimes that are promoted for repeated use can use `oomwrap wrap` or
`oomwrap install-shims`. Apply these only to the approved runtime path. Verify
`oomwrap unwrap` or `oomwrap uninstall-shims` can restore the original command.
Shims and wrappers carry no limits; the policy file and the defaults decide
them. When `doctor` names a shim or wrapper of an earlier version, it fails
until you run the command that `doctor` gives.

## 8. Handle a pressure stop

Treat oomwrap exit code `137` as a stopped and invalid run, not as a model or
workload result. Preserve the event log and command log. The `stopped` event
names the reason:

- `job_limit`: the job reached its `--memory-max`, and the kernel killed in it;
- `job_pressure`: the job stalled on memory for `--job-pressure-for`;
- `outside_oom_kill`: the kernel's out-of-memory killer, acting for a limit
  outside the job, killed a job process;
- `domain_floor`: the free room fell below `--min-mem`;
- `domain_pressure`: the domain was short of memory and stalled.

Exit `4` means that preflight refused the launch because there is no room now,
and exit `3` that oomwrap cannot guard as asked, such as a missing machine-wide
guard or an explicit limit without the `cgroup` backend. Report the observed
RAM, swap, process state, and last completed durable output.

On macOS there is no oomwrap event log. Preserve the server log and the pressure
readings, and treat a deliberate stop of the launch as a stopped run too.

Do not retry unchanged. Reduce the workload, free an approved resource, select a
smaller official artifact, or ask for a changed method.

For unified-memory inference failures, read
`references/unified-memory-inference-recovery.md`. A GPU reset or reboot requires
explicit approval because it can close the graphical session and interrupt
other work.

## 9. Report completion

Report:

- exact guarded command and oomwrap version;
- the backend, the resolved limits from the `launched` event, and the lowest
  observed headroom;
- child exit status and event-log path;
- staged checks completed;
- for inference, requested and observed backend details;
- any pressure stop, fallback, or unresolved risk.
