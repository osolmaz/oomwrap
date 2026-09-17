# oomwrap

<p align="center">
  <img src="assets/cover.svg" alt="oomwrap wraps an agent-launched inference server, watches RAM and swap floors, and stops its process group before the machine freezes" width="880">
</p>

oomwrap is a Linux command-line guard for agents that launch local inference
engines and other memory-heavy jobs. It wraps one command, watches available RAM
and swap, and stops the owned process group before memory pressure freezes the
machine.

Use it when an agent starts vLLM, SGLang, llama.cpp, or another inference
engine. It also protects builds, renderers, and data jobs. oomwrap complements
machine-wide tools such as `earlyoom`; it does not replace them.

## Install

Install oomwrap from crates.io:

```bash
cargo install oomwrap --locked
```

Install the latest `main` branch from GitHub:

```bash
cargo install --git https://github.com/osolmaz/oomwrap --locked
```

For development from a local checkout:

```bash
cargo install --path . --locked
```

Check the machine and installation:

```bash
oomwrap doctor
oomwrap inspect
```

## Run a command

Choose RAM and swap floors for the machine and workload:

```bash
oomwrap run \
  --profile generic \
  --min-mem 8G \
  --min-swap 1G \
  -- ./build-large-project.sh
```

oomwrap launches the child in its own process group. It forwards terminal
signals and returns the child exit code on a normal exit. If memory pressure
crosses a floor, oomwrap sends `SIGTERM`, waits for the configured grace period,
and then sends `SIGKILL` to the remaining process group. A pressure stop returns
exit code `137`.

## Use with earlyoom

[`earlyoom`](https://github.com/rfjakob/earlyoom) is a Linux user-space daemon
that acts before the kernel's out-of-memory killer. It checks available memory
and free swap several times per second. By default, when both fall below its
configured thresholds, it selects the process with the highest kernel
`oom_score` and sends `SIGTERM`. It sends `SIGKILL` at a lower emergency
threshold and continues until memory pressure recovers.

This is machine-wide protection. `earlyoom` can stop any process selected by its
policy, including a process outside the current workload. Its thresholds and
process preference rules are configurable.

`oomwrap` protects a narrower scope. It watches only the process group that it
starts, uses explicit available RAM and free swap floors, and stops that whole
group when a floor is crossed. This makes cleanup predictable for an inference
server and its child processes.

Use both on a workstation. Set the oomwrap floors above the emergency levels
used by `earlyoom` so oomwrap can stop the guarded workload first. `earlyoom`
then remains the machine-wide fallback for other processes and unexpected
memory pressure.

Check both tools with:

```bash
systemctl is-active earlyoom
systemctl status earlyoom
oomwrap doctor
```

When `earlyoom` runs as a system service, inspect its actions with:

```bash
sudo journalctl -u earlyoom | grep sending
```

oomwrap does not install or configure `earlyoom`. Neither tool replaces the
need to leave enough memory headroom for the operating system and temporary
allocation peaks.

## Run inference engines

Choose the profile that matches the inference engine, then put the engine
command after `--`:

```bash
oomwrap run \
  --profile sglang \
  --min-mem 24G \
  --min-swap 4G \
  -- python -m sglang.launch_server ...
```

Supported profiles are `auto`, `vllm`, `llama-cpp`, `sglang`, `trtllm`, `tgi`,
and `generic`. The inference profiles require active `earlyoom` by default. Use
`generic` for other commands. Use `--allow-no-earlyoom` only for tests or a
controlled machine that has another machine-wide safety mechanism.

## Event logs

Write launch, refusal, exit, and memory-pressure events as JSON Lines:

```bash
oomwrap run \
  --event-log ./oomwrap.events.jsonl \
  --profile generic \
  --min-mem 8G \
  --min-swap 1G \
  -- ./memory-heavy-command
```

## Inference command wrappers

PATH shims let existing inference commands run through oomwrap without changes
to each script:

```bash
oomwrap install-shims
```

This installs shims for common inference tools under `~/.local/bin`. Put that
directory before the real runtime directory in `PATH`.

Remove the shims with:

```bash
oomwrap uninstall-shims
```

Use `wrap` when a benchmark or script calls a fixed runtime path:

```bash
oomwrap wrap ~/runtimes/vllm/current/.venv/bin/vllm
```

This moves the original executable to `vllm.real` and places an oomwrap-managed
wrapper at the original path. Restore it with:

```bash
oomwrap unwrap ~/runtimes/vllm/current/.venv/bin/vllm
```

## Exit behavior

- Child exits normally: return the child exit code.
- Required `earlyoom` is missing: return `3`.
- A preflight memory or swap floor fails: return `4`.
- oomwrap stops the group for memory pressure: return `137`.
- Configuration or supervision fails: return `2`.

## Agent skill

The canonical `memory-safe-launch` skill is in
[`.agents/skills/memory-safe-launch`](.agents/skills/memory-safe-launch). It
describes a safe launch procedure for general memory-heavy commands and adds
checks for local model inference.

The binary exposes the skill through
[Skillflag](https://github.com/osolmaz/skillflag):

```bash
oomwrap --skill list
oomwrap --skill show memory-safe-launch
oomwrap --skill export memory-safe-launch > memory-safe-launch.tar
```

The skill is embedded in the binary, so these commands also work after a
`cargo install` without the source checkout.

## License

[MIT](LICENSE)
