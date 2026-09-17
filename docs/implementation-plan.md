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

`oomwrap run`:

- reads `/proc/meminfo` before launch;
- checks configured available-RAM and free-swap floors;
- starts the child in a new process group;
- forwards terminal signals;
- monitors the child and both memory floors;
- sends `SIGTERM`, then `SIGKILL`, to the process group on pressure;
- writes optional JSON Lines events for launches, refusals, pressure stops, and
  exits;
- returns `137` when it stops a group for memory pressure.

A `generic` profile supports any command. Inference profiles add command
detection and require active `earlyoom` by default because large local model
loads can also create driver-level allocation pressure.

## CLI surface

```bash
oomwrap doctor
oomwrap inspect
oomwrap run [options] -- <command> [args...]
oomwrap install-shims [options]
oomwrap uninstall-shims [options]
oomwrap wrap <path>
oomwrap unwrap <path>
```

Important run options:

```text
--profile auto|vllm|llama-cpp|sglang|trtllm|tgi|generic
--min-mem 24G
--min-swap 4G
--poll 1s
--term-grace 10s
--require-earlyoom | --allow-no-earlyoom
--event-log PATH
```

The shim and wrapper environment uses the `OOMWRAP_*` prefix.

## Adoption paths

General commands call `oomwrap run` directly.

Inference runtimes can also use PATH shims for known executable names or an
in-place wrapper for a fixed executable path. The original executable is kept as
a sidecar and `oomwrap unwrap` restores it exactly.

No shim or wrapper is installed without an explicit command.

## Safety boundaries

The guard rejects a launch when the configured memory or swap floor already
fails. High-risk inference profiles reject a launch when `earlyoom` is required
but not active. The `generic` profile lets the operator select the needed
machine-wide policy explicitly.

A guard error after the child starts must clean up the complete process group.
Terminal foreground control must return to the caller after normal exit,
pressure stop, interruption, or internal error.

## Testing

Unit tests cover:

- memory-size and duration parsing;
- profile selection and command matching;
- event serialization;
- shim and wrapper generation;
- process-state helpers.

Integration tests cover:

- normal and nonzero child exits;
- signal exit codes;
- preflight refusal;
- pressure stops with fake memory input;
- process-group cleanup;
- signal forwarding;
- PATH shim resolution without recursion;
- wrapper and unwrap round trips;
- required `earlyoom` behavior.

CI must not induce a real out-of-memory condition. Tests use fake memory files
and bounded child processes.

## Packaging

The current installation paths are `cargo install --git` and `cargo install
--path`. A later release can add signed GitHub release artifacts and package
manager integrations. Release work is separate from the rename and skill
cutover.
