---
name: memory-safe-launch
description: Use before launching a local command that can consume large RAM or swap, including builds, renderers, data jobs, model loads, and inference servers. Uses oomwrap for process-group cleanup, memory preflight, pressure stops, staged checks, and recovery.
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
owns one new child process group. It does not adopt a process that is already
running.

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

Stop when an unrelated large process already makes the launch unsafe. Do not
terminate another agent or process without explicit approval.

On macOS, skip the oomwrap check and installation below. The `oomwrap` binary is
Linux-only, and macOS does not use `earlyoom`.

If oomwrap is absent, install it only when installation is authorized:

```bash
cargo install --git https://github.com/osolmaz/oomwrap --locked
```

Verify installation with `oomwrap inspect`.

## 4. Set memory floors

Estimate the workload's steady-state use and temporary peak. Keep enough RAM and
swap for the operating system, terminal, browser, and other active work.

Use the repository or operator floors when they exist. Otherwise, propose floors
and explain the remaining headroom before launch. Treat the oomwrap defaults of
24 GiB available RAM and 4 GiB free swap as conservative large-workload values,
not as universal tuning targets.

Lower an established floor only when the user explicitly approves the named
command and values. An override must not disable the process-group guard. Keep
machine-wide protection active for a high-risk launch.

## 5. Launch through oomwrap

Use the generic profile for builds, rendering, conversion, extraction, and other
commands:

```bash
oomwrap run \
  --profile generic \
  --min-mem '<approved RAM floor>' \
  --min-swap '<approved swap floor>' \
  --event-log '<durable event-log path>' \
  -- <command> [args...]
```

Known inference-engine commands are detected automatically:

```bash
oomwrap run \
  --min-mem '<approved RAM floor>' \
  --min-swap '<approved swap floor>' \
  --event-log '<durable event-log path>' \
  -- llama-server --model '<model path>'
```

On Linux, inference profiles require active `earlyoom` by default. Keep
`earlyoom` as the machine-wide safety net. oomwrap is the process-scoped guard.
Do not use `--allow-no-earlyoom` for a large model load.

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

## 8. Handle a pressure stop

Treat oomwrap exit code `137` as a stopped and invalid run, not as a model or
workload result. Preserve the event log and command log. Report the observed RAM,
swap, process state, and last completed durable output.

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
- selected floors and lowest observed headroom;
- child exit status and event-log path;
- staged checks completed;
- for inference, requested and observed backend details;
- any pressure stop, fallback, or unresolved risk.
