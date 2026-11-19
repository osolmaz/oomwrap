# Plan: the cgroup guard

**TL;DR.** oomwrap 0.2.0 runs each guarded command in its own cgroup v2 group, through a transient
systemd scope. The kernel then enforces the job's memory limit on every allocation, kills the whole
job together, and records why. oomwrap stops the job early on memory pressure, which it reads from
the kernel's pressure measure (PSI), and no longer on free swap. Every default is a share of the
machine's RAM, so one configuration fits a laptop and a large server. Where no cgroup can be made,
oomwrap falls back to the current polling guard with the same policy. The change is a hard cutover:
`--min-swap` and `--use-systemd-scope` go away. After 0.2.0, all guarded jobs share one kernel
budget, wait for room instead of failing, and react to kernel events instead of polling
([The end state](#the-end-state)).

## Why

The goal stays the one in [implementation-plan.md](implementation-plan.md): stop a memory-heavy
command before memory pressure freezes the machine. The current design misses that goal in five
ways:

| Fault of 0.1.0 | Effect |
| --- | --- |
| Fixed floors (`24G` RAM, `4G` swap) | Too strict on a small machine, too loose on a large one; each machine needs its own numbers |
| The swap floor stops a job on its own (`run_guard`, the `SwapFree` check) | Linux moves idle pages to swap while much RAM stays available. Free swap then falls below the floor and oomwrap stops a healthy job, with no pressure at all |
| Polling every second | A fast allocation can pass the floor and freeze the machine between two readings |
| The stop signals the process group | A child that calls `setsid()`, as servers, browsers and daemons do, leaves the group and survives the stop |
| No record from the kernel | The event log shows oomwrap's own reading, never what the kernel did |

A machine freezes when the processes spend their time waiting for memory: reclaim and swap traffic,
not a low swap counter. The kernel measures that waiting directly in its pressure stall information
(`/proc/pressure/memory`, and `memory.pressure` for each cgroup), the same signal that
`systemd-oomd` uses.

## Design

### Two backends

| Backend | When | Mechanism |
| --- | --- | --- |
| `cgroup` | A user systemd manager runs, cgroup v2 is mounted, and the memory controller is delegated to the user | Transient scope with kernel-enforced limits; stop through `cgroup.kill` |
| `poll` | Any Linux machine | Today's process-group guard with the new policy |

`--backend auto` (the default) takes `cgroup` when the checks pass and `poll` otherwise, and logs
which one it chose and why. `--backend cgroup` refuses to start without cgroup support (exit 3, as a
missing required `earlyoom` does). A job that runs inside another oomwrap job (`OOMWRAP_UNIT` is set)
uses `poll`: a new scope would be a sibling under the user manager and would leave the outer job's
limits.

### Launch in a transient scope

```bash
systemd-run --user --scope --quiet --collect \
  --unit oomwrap-<profile>-<pid>-<random> \
  -p MemoryMax=<bytes> -p MemoryHigh=<bytes> -p MemorySwapMax=<bytes> \
  -p OOMPolicy=kill \
  -- <oomwrap> exec-job --oom-score-adj 1000 -- <command> [args...]
```

oomwrap spawns `systemd-run` in a new process group, as it spawns the command today, so terminal
foreground control and signal forwarding stay as they are. `systemd-run --scope` creates the scope
with its own PID and then executes the next program in that same process, so no process of the job
ever runs outside the cgroup. `OOMPolicy=kill` sets `memory.oom.group`, so a kernel kill at
`MemoryMax` ends the whole job at once.

**The job is the first victim.** `exec-job` is a hidden subcommand of oomwrap itself. It writes
`1000` to `/proc/self/oom_score_adj` and then executes the command in the same process, so every
process of the job inherits the score. A normal user may raise its own score, and safe Rust does
both steps. The kernel's out-of-memory killer and `earlyoom` choose their victim by this score, so a
machine-wide kill takes the guarded job before an editor, a browser or an agent session. The `poll`
backend starts the job through `exec-job` too.

After the start, oomwrap:

1. reads `/proc/<pid>/cgroup` until it names `<unit>.scope`, for at most 5 s;
2. reads `memory.max`, `memory.high`, `memory.swap.max` and `memory.oom.group` back from the cgroup
   and checks that they hold the requested values. If not, the memory controller is not delegated:
   `auto` stops the scope and starts again with `poll`, `cgroup` stops with exit 3;
3. exports `OOMWRAP_UNIT=<unit>` to the job, for nested runs.

Rejected alternatives:

- **D-Bus `StartTransientUnit` through `zbus`.** It is the API under `systemd-run`, but it needs a
  large async dependency for one call, and its own handshake to keep the child out of the cgroup
  race. `systemd-run` already does both.
- **Writing `/sys/fs/cgroup` directly.** systemd owns the tree, and a terminal's scope is not
  delegated; writing there breaks systemd's single-writer rule.
- **`cgexec` and libcgroup.** Not installed by default, and designed for cgroup v1.

### Limits

Every size takes bytes (`24G`) or a share of `MemTotal` (`85%`). oomwrap resolves shares itself and
passes bytes to systemd, so the events, `doctor` and both backends show the same numbers.

| Option | Default | Meaning |
| --- | --- | --- |
| `--memory-max` | `85%` | Hard limit of the job (`memory.max`). The kernel kills the job when reclaim cannot keep it below |
| `--memory-high` | `80%` | Soft limit (`memory.high`). The kernel slows the job and reclaims its pages first |
| `--memory-swap-max` | `0` | The job's own swap (`memory.swap.max`). `0` keeps a job from thrashing the disk; a job that fills its limit is killed instead of slowing the machine |
| `--min-mem` | `15%` | Machine floor for `MemAvailable`, in both backends; above `earlyoom`'s default of 10% so oomwrap acts first |
| `--max-pressure` | `40%` | PSI `full avg10` limit for the job and for the machine |
| `--pressure-for` | `10s` | How long the pressure must stay above the limit |

The PSI defaults are starting values. Phase 5 calibrates them with recorded runs before the release.
`doctor` prints the resolved values for the machine. Validation refuses `memory-high` above
`memory-max` and a floor that is not below `MemTotal`.

### Stop conditions

Each poll builds one `Readings` value: machine `MemTotal`, `MemAvailable`, machine PSI, and, in the
`cgroup` backend, the job's `memory.current`, `memory.swap.current`, `memory.pressure` and
`memory.events`. One pure function decides:

| Reason | Condition | Backend |
| --- | --- | --- |
| `kernel_oom_kill` | `oom_kill` or `oom_group_kill` in `memory.events` rose | `cgroup` |
| `job_pressure` | The job's `full avg10` stayed at or above `--max-pressure` for `--pressure-for` | `cgroup` |
| `machine_pressure` | The machine's `full avg10` stayed at or above `--max-pressure` for `--pressure-for` | both |
| `ram_floor` | `MemAvailable` fell below `--min-mem` | both |

Free swap is no stop condition. A full swap with available RAM is no danger, and low RAM already
stops the job through `ram_floor`. The job's own swap is set by `--memory-swap-max`. Where the kernel
has no PSI, the pressure conditions are off and `doctor` says so.

Preflight refuses a launch (exit 4) when `ram_floor` or `machine_pressure` already holds.

Memory that a device driver allocates for the job can sit outside these readings. On a machine with
unified GPU memory, where the GPU has no memory of its own, a model's GPU allocations come from
system RAM, and the memory controller may not charge them to the job's cgroup. `machine_pressure`
and `ram_floor` still see that memory. Phase 5 measures whether the cgroup charges it, and step 12
of [The end state](#the-end-state) counts it in the job's limit.

### Machine-wide guard

oomwrap guards one job; a machine-wide guard stays the fallback for everything else. The inference
profiles require one: `earlyoom` or `systemd-oomd`, whichever is active. `--require-machine-guard`
and `--allow-no-machine-guard` replace `--require-earlyoom` and `--allow-no-earlyoom`, and a missing
required guard keeps exit 3. oomwrap never depends on `systemd-oomd` for its own decisions, because
many machines do not install it. When both guards are active, `doctor` warns that two tools may kill
for the same pressure.

### Stop sequence

`cgroup`: send `SIGTERM` to every PID in `cgroup.procs`, wait `--term-grace`, write `1` to
`cgroup.kill`, and wait until `cgroup.procs` is empty. `cgroup.kill` reaches every process in the
group, also one that left the process group. systemd removes the scope (`--collect`).

`poll`: today's process-group sequence.

Both return 137 for a guard stop, and the event names the reason. A `kernel_oom_kill` also returns
137: the kernel ended the job at its limit, which is a pressure stop.

### Events

Every event gains `backend`, and in the `cgroup` backend `unit` and `cgroup`. `launched` holds the
resolved limits in bytes. A stop event holds the `reason` from the table above and the readings that
decided it: PSI values, `memory.current` and the `memory.events` counters.

### doctor and inspect

`doctor` reports cgroup v2, the user manager, the delegated controllers, PSI for the machine, the
backend that `auto` would take and why, the resolved limits, and the machine-wide guards: which of
`earlyoom` and `systemd-oomd` are active, a warning when `earlyoom`'s threshold is not below
`--min-mem`, and a warning when both run. It also finds shims and wrappers that an earlier version
made, which pass flags that 0.2.0 no longer has, and names the command that writes them again.

### Configuration

Every call site passes its own floors today, so one machine's policy is copied into each script,
shim and wrapper. 0.2.0 reads one policy file: `/etc/oomwrap.toml`, then
`~/.config/oomwrap/config.toml`, each with a section for each profile. Flags override the file. A
value missing in all of them takes the defaults above. `doctor` prints the merged policy and the
file that set each value.

Shims and wrappers carry no policy. Both are made by one generator and end in
`exec oomwrap run --shim <tool> -- "$real" "$@"`, so a later change of flags or defaults never
breaks an installed shim.

### Code structure

`src/lib.rs` (1,851 lines) splits by job, so that the guard core keeps no CLI, shim or file
system code (`AGENTS.md`):

| Module | Holds | IO |
| --- | --- | --- |
| `limits` | `Size` (bytes or share of `MemTotal`), resolution and validation | none |
| `readings` | Parsers of `meminfo`, PSI, `memory.events`, `memory.current` | none |
| `host` | `Host`: the proc root, the cgroup root and the `earlyoom` check, built once at the border; tests build a fake one | file reads |
| `policy` | `Decision::evaluate(&Limits, &Readings, &mut PressureWindow)` and `StopReason` | none |
| `backend` | `enum Backend { Cgroup(Scope), Poll(ProcessGroup) }`: launch, readings source, stop sequence | process and cgroup |
| `supervise` | The loop: signals, terminal foreground, events, and one `stop(StopReason)` path | yes |
| `config` | The policy file, its merge with flags, and the profile table | file reads |
| `shims`, `events`, `doctor`, `cli` | Today's code, moved | as today |

`Backend` is an enum, not a trait object: the set is closed at two.

Smells of 0.1.0 that the split removes:

| Smell (place in 0.1.0) | Change |
| --- | --- |
| The child-exit block is copied twice in `run_guard`, and `kill_for_pressure`, `kill_for_parent_signal` and `cleanup_child_after_error` are near copies | One `stop(StopReason)` path; the loop returns one `Outcome` |
| Event names and labels are strings (`Event`), and exit codes are bare integers in five places | `enum EventKind`, and `enum Outcome` with one `exit_code()` |
| Sizes and durations stay `String` in the argument structs and are parsed inside `run_guard` | clap value parsers give `Size` and `Duration` at the border |
| Engine names sit in two separate lists (`detect_profile`, `known_high_memory_processes`) | One profile table: executables, Python modules and default policy |
| Test seams are environment variables read deep in the code (`OOMWRAP_MEMINFO_PATH`, `OOMWRAP_EARLYOOM_ACTIVE`) | One `Host` value passed in; the variables are read once, at the border |
| `parse_size` reads `GB` as GiB and computes through `f64` | Binary units `K`, `M`, `G` and `T` only, as systemd reads them, whole numbers or a percentage; `GB` is refused |
| Two near-copy bash generators bake the flags into every shim and wrapper | One generator, no policy in the script (above) |

## Tests

- **Unit:** `Size` resolution and its bounds; the PSI and `memory.events` parsers; one table test per
  `StopReason`; the pressure window, which must not stop on one high reading; and the regression
  test of the false stop: full swap, available RAM, no pressure, no stop.
- **Integration, in CI:** `OOMWRAP_PROC_ROOT` and `OOMWRAP_CGROUP_ROOT`, read once into `Host`,
  point at fake trees; they replace `OOMWRAP_MEMINFO_PATH`. The test changes the fake pressure and
  event files while a bounded child runs, and checks the reason and exit 137. The `poll` backend
  runs for real. A test reads `/proc/<child>/oom_score_adj` and expects `1000`.
- **Replay of recorded runs, in CI:** phase 5 records real runs as traces: one `Readings` value
  each second, with a generic job label (`build`, `renderer`, `model load`) and no command line,
  host name or path. The traces are stored under `tests/fixtures/traces/` with the decision that
  each run must get: a healthy run never stops, a run that froze the machine stops before the
  freeze. CI replays every trace through `policy`, so each change of the policy or of a default is
  checked against real history and not only against made-up numbers.
- **Real system, ignored by default** (`cargo test -- --ignored`, on a machine with a user systemd
  manager, never in CI): a helper binary under `tests/` that
  - allocates past a `64M` `--memory-max`: `kernel_oom_kill`, exit 137, no process left;
  - forks a child that calls `setsid()` and sleeps: the stop ends both;
  - runs at a `memory-high` far below its working set: `job_pressure`.

  The limits are a few megabytes, so the machine itself never comes under pressure. `RELEASING.md`
  makes this suite a release step.

## Phases

Each phase is one pull request that passes the checks of `AGENTS.md`.

1. **Split `lib.rs`** into the modules above, with `Host`, the profile table, `EventKind`,
   `Outcome`, one `stop` path and typed `Size` and `Duration` arguments. No change of behavior; the
   51 existing tests pass unchanged.
2. **Limits, readings and policy.** `Size` with shares and binary units only, the PSI parser, the
   policy and its tests. The `poll` backend moves to the new policy: no swap floor, machine
   pressure, shares of `MemTotal`. `exec-job` and the score of `1000`. The machine-wide guard
   check accepts `earlyoom` or `systemd-oomd`. CLI cutover: `--min-swap`, `--require-earlyoom` and
   `--allow-no-earlyoom` go.
3. **The `cgroup` backend:** scope launch, cgroup lookup and read-back, readings, stop sequence,
   `kernel_oom_kill`, `--backend`, nested runs, events. `--use-systemd-scope` goes; `--memory-max`
   and `--memory-high` get their meaning.
4. **Configuration, `doctor`, `inspect`, shims and wrappers:** the policy file, the capability
   report, the resolved limits, the one generator of shims and wrappers without policy, and the
   finding of old ones.
5. **Real-system suite, traces and calibration:** the ignored tests; recorded runs of a build, a
   renderer and a model load, stored as replay traces; the PSI defaults set from them; and a model
   load on a machine with unified GPU memory, to measure whether the cgroup charges the GPU's
   allocations. The results go into this document.
6. **Documents and release:** `README.md`, `implementation-plan.md` (its design section is replaced),
   the `memory-safe-launch` skill, `CHANGELOG.md`, version 0.2.0. The release workflow also builds
   static binaries (`x86_64` and `aarch64`, musl) and attaches them to the GitHub release with
   SHA-256 checksums and build provenance attestations, so a machine without a Rust toolchain can
   install oomwrap. `cargo install` stays.

## The end state

0.2.0 guards one job well. The end state guards the machine's whole set of heavy jobs: each job
holds a memory contract that the kernel enforces, against one machine policy, with nothing to tune
per machine and every outcome explained. Each step below is its own release after 0.2.0.

7. **One shared budget.** Every guarded scope starts under one systemd slice, `oomwrap.slice`, with
   its own `MemoryMax` and `MemoryHigh` from the policy file. The kernel then enforces the total of
   all guarded jobs together, also of jobs that different users or scripts start.
8. **Wait for room instead of refusing.** A job may declare its need (`--expect 12G`, or a profile
   default). With `--wait-for-room`, a job whose need does not fit the slice and the machine waits,
   in order of arrival, until it fits, instead of failing at preflight (exit 4). This replaces
   hand-made queues that cap heavy jobs at a fixed count of slots. `oomwrap status` lists the jobs
   in the slice and the ones that wait. `oomwrap status --json` gives the budget, its use, the free
   room and the queue in a stable form, so a script or an agent can plan a launch before it starts
   one, instead of learning the limits from a refusal.
9. **Event-driven supervision.** One `poll()` loop on the child's `pidfd`, a `signalfd`, PSI
   triggers (the kernel wakes oomwrap when pressure passes a limit within a window, where it lets
   an unprivileged user register one; elsewhere oomwrap reads `avg10` as before), `cgroup.events`
   (the job is empty) and `memory.events` (the kernel killed). No poll interval to tune, and a
   reaction within milliseconds of the kernel's signal.
10. **Slow down before stopping.** `memory.high` already throttles a job before its hard limit.
    With `--on-pressure freeze`, a batch job is frozen (`cgroup.freeze`) while the machine is under
    pressure, and thawed when the pressure ends; it is stopped only when the pressure lasts longer
    than `--freeze-for`. A frozen job keeps its memory, so this helps only with another job's
    short peak.
11. **A stable event schema.** The JSON event log gets a version field, a documented schema and an
    `oomwrap explain` command that turns a log into the story of each stop: reason, readings and
    limits.
12. **Count device memory in the job.** When phase 5 shows that the cgroup does not charge a GPU's
    allocations from system RAM, oomwrap reads the job's GPU memory through the vendor's management
    library (NVML for NVIDIA, loaded at run time only when present) for every PID in
    `cgroup.procs`. It adds that memory to `memory.current` in `Readings`, and a new reason,
    `device_memory`, stops the job when the sum passes `--memory-max`. The kernel cannot enforce
    this part, so oomwrap does, at its reading interval.

What stays out:

- **Handing pressure stops to `systemd-oomd`.** Many machines do not install it, and oomwrap must
  work without it.
- **A Rust library API.** It adds public surface to keep stable, and a process cannot easily move
  itself into a new cgroup; the command line is the contract.
- **macOS.** It has no cgroups and no PSI. macOS stays out of scope, as in 0.1.0.

## Risks and open questions

- **Memory outside the cgroup's count.** A device driver can allocate memory for the job that the
  memory controller does not charge, for example unified GPU memory. `machine_pressure` and
  `ram_floor` stay for that reason; phase 5 checks a model load on such a machine, and step 12
  counts that memory in the job.
- **A job that lowers its own score.** An unprivileged process can lower its `oom_score_adj` again,
  down to the lowest value a privileged process set for it. A job that does so is no longer the
  first victim of a machine-wide kill; its own limits still hold.
- **No user manager.** Containers and SSH sessions without lingering have none. `auto` then uses
  `poll` and logs it.
- **systemd version.** Phase 3 sets the lowest version that applies `MemorySwapMax`, `MemoryHigh`
  and `OOMPolicy` to a scope, from the read-back, and `doctor` checks it.
- **PSI defaults.** `40%` for `10s` is a starting value until phase 5.
- **Breaking change.** Scripts that pass `--min-swap` fail with a clear error after the upgrade. The
  0.2.0 changelog says what replaces it.
