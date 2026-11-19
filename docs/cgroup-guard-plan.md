# Plan: the cgroup guard

**TL;DR.** oomwrap 0.2.0 runs each guarded command in its own cgroup v2 group, through a transient
systemd scope. The kernel then enforces the job's memory limit on every allocation, kills the whole
job together, and records why. oomwrap stops the job early on memory pressure, which it reads from
the kernel's pressure measure (PSI), and no longer on free swap. Every default is a share of the
machine's RAM, so one configuration fits a laptop and a large server. Where no cgroup can be made,
oomwrap falls back to the current polling guard with the same policy. The change is a hard cutover:
`--min-swap` and `--use-systemd-scope` go away.

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
  -- <command> [args...]
```

oomwrap spawns `systemd-run` in a new process group, as it spawns the command today, so terminal
foreground control and signal forwarding stay as they are. `systemd-run --scope` creates the scope
with its own PID and then executes the command in that same process, so no process of the job ever
runs outside the cgroup. `OOMPolicy=kill` sets `memory.oom.group`, so a kernel kill at `MemoryMax`
ends the whole job at once.

After the start, oomwrap:

1. reads `/proc/<pid>/cgroup` until it names `<unit>.scope`, for at most 5 s;
2. reads `memory.max`, `memory.high`, `memory.swap.max` and `memory.oom.group` back from the cgroup
   and checks that they hold the requested values. If not, the memory controller is not delegated:
   `auto` stops the scope and starts again with `poll`, `cgroup` stops with exit 3;
3. exports `OOMWRAP_UNIT=<unit>` to the job, for nested runs.

Rejected alternatives:

- **D-Bus `StartTransientUnit` through `zbus`.** It is the API under `systemd-run`, but it needs a
  large async dependency for one call, and a self-executing trampoline to keep the child out of the
  cgroup race. `systemd-run` already does both.
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
backend that `auto` would take and why, the resolved limits, and `earlyoom` with a warning when its
threshold is not below `--min-mem`. It also finds shims and wrappers that an earlier version made,
which pass flags that 0.2.0 no longer has, and names the command that writes them again.

### Code structure

`src/lib.rs` (1,851 lines) splits by job, so that the guard core keeps no CLI, shim or file
system code (`AGENTS.md`):

| Module | Holds | IO |
| --- | --- | --- |
| `limits` | `Size` (bytes or share of `MemTotal`), resolution and validation | none |
| `readings` | Parsers of `meminfo`, PSI, `memory.events`, `memory.current`; readers with injectable roots | file reads only |
| `policy` | `Decision::evaluate(&Limits, &Readings, &mut PressureWindow)` and `StopReason` | none |
| `backend` | `enum Backend { Cgroup(Scope), Poll(ProcessGroup) }`: launch, readings source, stop sequence | process and cgroup |
| `supervise` | The loop: signals, terminal foreground, events | yes |
| `shims`, `events`, `profiles`, `doctor`, `cli` | Today's code, moved | as today |

`Backend` is an enum, not a trait object: the set is closed at two.

## Tests

- **Unit:** `Size` resolution and its bounds; the PSI and `memory.events` parsers; one table test per
  `StopReason`; the pressure window, which must not stop on one high reading; and the regression
  test of the false stop: full swap, available RAM, no pressure, no stop.
- **Integration, in CI:** `OOMWRAP_PROC_ROOT` and `OOMWRAP_CGROUP_ROOT` point at fake trees, as
  `OOMWRAP_MEMINFO_PATH` does today. The test changes the fake pressure and event files while a
  bounded child runs, and checks the reason and exit 137. The `poll` backend runs for real.
- **Real system, ignored by default** (`cargo test -- --ignored`, on a machine with a user systemd
  manager, never in CI): a helper binary under `tests/` that
  - allocates past a `64M` `--memory-max`: `kernel_oom_kill`, exit 137, no process left;
  - forks a child that calls `setsid()` and sleeps: the stop ends both;
  - runs at a `memory-high` far below its working set: `job_pressure`.

  The limits are a few megabytes, so the machine itself never comes under pressure. `RELEASING.md`
  makes this suite a release step.

## Phases

Each phase is one pull request that passes the checks of `AGENTS.md`.

1. **Split `lib.rs`** into the modules above. No change of behavior; the 51 existing tests pass
   unchanged.
2. **Limits, readings and policy.** `Size` with shares, the PSI parser, the policy and its tests.
   The `poll` backend moves to the new policy: no swap floor, machine pressure, shares of
   `MemTotal`. CLI cutover: `--min-swap` goes.
3. **The `cgroup` backend:** scope launch, cgroup lookup and read-back, readings, stop sequence,
   `kernel_oom_kill`, `--backend`, nested runs, events. `--use-systemd-scope` goes; `--memory-max`
   and `--memory-high` get their meaning.
4. **`doctor`, `inspect`, shims and wrappers:** the capability report, the resolved limits, new
   shim and wrapper scripts (`OOMWRAP_MEMORY_MAX` and the others replace `OOMWRAP_MIN_SWAP`), and
   the finding of old ones.
5. **Real-system suite and calibration:** the ignored tests, then recorded runs of a build, a
   renderer and a model load, to set the PSI defaults. The results go into this document.
6. **Documents and release:** `README.md`, `implementation-plan.md` (its design section is replaced),
   the `memory-safe-launch` skill, `CHANGELOG.md`, version 0.2.0.

## Risks and open questions

- **Memory outside the cgroup's count.** A device driver can allocate memory for the job that the
  memory controller does not charge, for example unified GPU memory. `machine_pressure` and
  `ram_floor` stay for that reason; phase 5 checks a model load on such a machine.
- **No user manager.** Containers and SSH sessions without lingering have none. `auto` then uses
  `poll` and logs it.
- **systemd version.** Phase 3 sets the lowest version that applies `MemorySwapMax`, `MemoryHigh`
  and `OOMPolicy` to a scope, from the read-back, and `doctor` checks it.
- **PSI defaults.** `40%` for `10s` is a starting value until phase 5.
- **Breaking change.** Scripts that pass `--min-swap` fail with a clear error after the upgrade. The
  0.2.0 changelog says what replaces it.
