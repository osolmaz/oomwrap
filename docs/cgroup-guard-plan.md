# Plan: the cgroup guard

**TL;DR.** oomwrap 0.2.0 runs each guarded command in its own cgroup v2 group, through a transient
systemd scope, when the machine allows it. The kernel then enforces the job's memory limit on every
allocation, and oomwrap ends the whole job, also processes that left its process group. oomwrap
stops a job on memory pressure, which it reads from the kernel's pressure measure (PSI), and no
longer on free swap. All limits are shares of the job's memory domain (the machine, or the container
or limited cgroup that the caller runs in), with absolute bounds, so the same policy fits a small
board, a laptop, a large server and a CI container. Where no scope can be made, oomwrap uses the
polling guard with the same policy. The command starts once, only after its limits hold. The change
is a hard cutover to new flags. After 0.2.0, all guarded jobs of a user share one kernel budget,
wait for room instead of failing, and react to kernel events instead of polling
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

### The memory domain

The memory domain is the memory that the job competes for. Of the machine and every cgroup at or
above the caller that has a `memory.max`, it is the one with the smallest size. In a container or a
limited service, `/proc/meminfo` and `/proc/pressure/memory` describe the host, not the memory that
binds, so the domain decides what oomwrap reads:

| Reading | Machine domain | Cgroup domain |
| --- | --- | --- |
| Size | `MemTotal` minus `Hugetlb` | `memory.max` |
| Free room | `MemAvailable` | `memory.max` − `memory.current` + `inactive_file` from `memory.stat` |
| Pressure | `/proc/pressure/memory` | the domain's `memory.pressure` |

The free room is the smallest free room of the machine and of every limited cgroup on the path,
because the job can grow only until the first limit binds. The closest limited cgroup is not always
that limit: an outer cgroup can have a smaller `memory.max`, or siblings that use most of a larger
one, and a `memory.max` above the machine's RAM never binds. (The first version of this plan took
the closest cgroup; phase 2 changed it for these cases.)

Every share in the policy (`85%`, `15%`) is a share of the domain's size. A job's own limit can
never exceed its domain. Reserved huge pages are not usable by the job, so the machine domain leaves
them out. `MemAvailable` does not count a ZFS ARC as free; `doctor` names that case.

### Two backends

| Backend | Mechanism |
| --- | --- |
| `cgroup` | Transient scope with kernel-enforced limits; stop through `cgroup.kill` |
| `poll` | The process-group guard of 0.1.0, with the new policy and the memory domain |

`--backend auto` (the default) takes `cgroup` when the checks below pass, and `poll` otherwise, and
logs which one it chose and why. `--backend cgroup` exits with 3 when a check fails. oomwrap checks
only what the launch cannot test; the launch handshake tests the rest (bus, delegation, limits):

1. **cgroup v2 only.** `/proc/self/mountinfo` shows a `cgroup2` mount and no cgroup v1 controller
   mounts. A hybrid host uses `poll`.
2. **`cgroup.kill` exists** (Linux 5.14 or later). There is one stop path, with no loop for older
   kernels.
3. **The job never leaves its caller's limits.** A user scope starts under the user manager,
   outside the caller's cgroup. oomwrap reads `memory.max`, `memory.high` and `memory.swap.max` of
   each cgroup that the job would leave: from the caller's cgroup up to, and not including, the
   closest cgroup that also holds the user manager. When one of them holds a limit, the memory
   domain is that cgroup and the job stays in place with `poll`. A nested oomwrap job, a container,
   a limited service and a CI job are all cases of this one rule.
4. **The job survives the caller's logout as before.** A user manager is stopped when the user's
   last session ends, unless lingering is on (`pam_systemd`). A scope under it would be stopped too,
   where a job under `nohup`, `tmux` or `screen` survives today. So `cgroup` needs lingering, or a
   caller that itself runs inside the user manager and so ends with it anyway. `doctor` names
   `loginctl enable-linger`. A caller running as root uses the system manager
   (`systemd-run --scope` without `--user`), which needs no lingering.

**Explicit limits are a requirement.** A `--memory-max`, `--memory-high` or `--memory-swap-max`
from a flag or the policy file needs the `cgroup` backend: when it is not available, `auto` exits
with 3 and names the failed check, instead of running the job without the limit. Default limits are
best effort: in `poll`, the domain's stop conditions apply, and the `launched` event says that no
job limit holds.

### Launch in a transient scope

```bash
systemd-run --user --scope --quiet --collect \
  --unit oomwrap-<profile>-<random> \
  -p MemoryMax=<bytes> -p MemoryHigh=<bytes> [-p MemorySwapMax=<bytes>] \
  -- /proc/<guard pid>/exe exec-job --control-fd <n> -- <command> [args...]
```

oomwrap spawns `systemd-run` in a new process group, as it spawns the command today, so terminal
foreground control and signal forwarding stay as they are. `systemd-run --scope` creates the scope
with its own PID and then executes the next program in that same process. The job therefore
inherits the caller's environment, working folder and terminal, and no process of the job ever runs
outside the cgroup. The program is `/proc/<guard pid>/exe`, the running guard's own binary, so an
upgrade that replaces the oomwrap file between launch and `exec-job` cannot mix two versions.

Minimum systemd: 236 (`--collect`; `MemoryMax` and `MemoryHigh` came in 231, `MemorySwapMax` in
232). oomwrap does not set `OOMPolicy`, which scopes accept only from systemd 253; it ends the job
itself when the kernel kills inside it ([Stop conditions](#stop-conditions)).

Sizes are rounded down to a whole number of pages (the page size from `sysconf`, which is not 4 KiB
on every machine) before they go to systemd, because the kernel stores limits in pages.

A probe on systemd 255 confirmed the assumptions of this section: an inherited descriptor reaches
the program that `systemd-run --scope` executes; that program runs in the scope's cgroup; the kernel
reads `MemoryMax=100000001` back as `99999744`, rounded down to 4 KiB pages; without `OOMPolicy`,
`memory.oom.group` stays `0` and the job inherits the user manager's `oom_score_adj`; and a program
started as `/proc/<pid>/exe` runs. The CI runner matrix repeats these checks on systemd 249.

**The command starts once, after the check.** `exec-job` is a hidden subcommand of oomwrap. Before
the launch, oomwrap makes a Unix socket pair and passes one end to `exec-job` as `--control-fd`.

1. `exec-job` writes `1000` to `/proc/self/oom_score_adj`, reads `/proc/self/cgroup`, and sends a
   protocol version and the cgroup path.
2. oomwrap checks the version and that the path names `<unit>.scope`, and reads `memory.max`,
   `memory.high` and, when set, `memory.swap.max` back from that cgroup, compared in pages.
3. When all of them hold the requested values, oomwrap answers `go`. `exec-job` marks the socket
   close-on-exec and executes the command. End of file on the socket tells oomwrap that the exec
   worked. When the exec fails, `exec-job` sends the error and exits; oomwrap reports exit 127 (not
   found) or 126 (cannot run), as a shell does, never as the command's own exit code.
4. When the check fails, oomwrap answers `abort`, `exec-job` exits without starting the command,
   and `auto` starts the command once with `poll` while `cgroup` exits with 3.
5. When the socket closes, or 5 s pass, before the report, `systemd-run` or `exec-job` failed before
   the command. oomwrap kills and reaps the launch's process group, then applies the rule of
   step 4. A user bus that a cron job cannot reach ends here.
6. A signal to oomwrap during the handshake kills and reaps the launch and ends oomwrap with
   128 + the signal number. The command never starts.

The `poll` backend starts the job through `exec-job` too, with the same handshake but without the
cgroup check.

**The job is the first victim.** The score of `1000` from step 1 is inherited by every process of
the job. A normal user may raise its own score, and safe Rust does both steps. The kernel's
out-of-memory killer and `earlyoom` choose their victim by this score, so a machine-wide kill takes
the guarded job before an editor, a browser or an agent session.

**Each guard holds a lock.** oomwrap holds a lock file, `oomwrap/<unit>.lock` in
`$XDG_RUNTIME_DIR` (or `/run/oomwrap/` for root), for its whole run. A scope `oomwrap-*` whose lock
nobody holds has lost its guard. A lock works where a PID in the unit name fails after PID reuse.

Rejected alternatives:

- **D-Bus `StartTransientUnit` through `zbus`.** It is the API under `systemd-run`, but it needs a
  large async dependency for one call, and its own handshake to keep the child out of the cgroup
  race. `systemd-run` already does both.
- **Writing `/sys/fs/cgroup` directly.** systemd owns the tree, and a terminal's scope is not
  delegated; writing there breaks systemd's single-writer rule.
- **`cgexec` and libcgroup.** Not installed by default, and designed for cgroup v1.
- **Reading back the limits after the command started, and starting again on a failed check.** A
  build or a model load would then run twice.
- **`OOMPolicy=kill`.** It would exclude every distribution with systemd older than 253.

### Limits

A size is bytes with a binary unit (`512M`, `24G`), a share of the domain's size (`85%`), or
`infinity`. A default may carry absolute bounds.

| Option | Default | Meaning |
| --- | --- | --- |
| `--memory-max` | smallest of: `85%`; the domain's free room at launch minus `--min-mem`; the lowest `memory.max` above the scope | Hard limit of the job (`memory.max`). The kernel reclaims, then kills inside the job |
| `--memory-high` | `90%` of the resolved `--memory-max` | Soft limit (`memory.high`). The kernel slows the job and reclaims its pages first |
| `--memory-swap-max` | not set | The job's own swap (`memory.swap.max`). Not set, the kernel's normal swap behavior applies |
| `--min-mem` | `15%`, at least `256M`, at most `16G` | Floor for the domain's free room |
| `--job-pressure`, `--job-pressure-for` | `60%`, `30s` | PSI `full avg10` limit of the job's cgroup, and how long it must last |
| `--domain-pressure`, `--domain-pressure-for` | `20%`, `10s` (provisional) | PSI `full avg10` limit of the domain, and how long it must last |

Why these defaults:

- **The hard limit is capped by the free room at launch.** With `85%` alone, the kernel limit would
  bind only when nothing else used RAM, and the floor, read once a second, would act first, with the
  same gap as 0.1.0 between two readings. The cap is a snapshot at launch; the floor still guards
  what other processes do later. When the cap is zero or less, preflight refuses the launch (exit
  4).
- **No job swap limit by default.** At a swap limit the kernel stops swapping the job's anonymous
  pages, so global reclaim swaps out the editor and the browser instead. A swap limit of `0` would
  also shut out zram and zswap, the only swap on many small machines. Thrashing in swap shows as
  pressure, which `job_pressure` and `domain_pressure` catch.
- **The floor has bounds.** `15%` alone is 150 MiB on a 1 GiB board and 300 GiB on a 2 TiB server.
- **Job pressure** uses the published defaults of `systemd-oomd` (`DefaultMemoryPressureLimit=60%`,
  `DefaultMemoryPressureDurationSec=30s`), which apply to one unit's cgroup, as a job is.
- **Domain pressure** is lower and shorter, because `60%` for `30s` at the root of a machine is
  already a frozen desktop. The value is provisional until phase 6 records real machine pressure on
  throwaway virtual machines.

When only one of `--memory-high` and `--memory-max` is given, the other follows by the default
ratio. An explicit `--memory-max` is used as given, also above the launch-time cap; preflight still
refuses the launch when `domain_floor` holds. Validation refuses `memory-high` above `memory-max`
and a floor that is not below the domain's size. `doctor` prints every resolved value and where it
came from.

### Stop conditions

Each reading builds one `Readings` value: the domain's size, free room and PSI, and, in the `cgroup`
backend, the job's `memory.current`, `memory.swap.current`, `memory.pressure` and `memory.events`.
One pure function decides:

| Reason | Condition | Backend |
| --- | --- | --- |
| `job_limit` | `oom` in the job's `memory.events` rose: the job reached `memory.max` | `cgroup` |
| `outside_oom_kill` | `oom_kill` rose without `oom`: an out-of-memory killer outside the job's limit killed a job process | `cgroup` |
| `job_pressure` | The job's `full avg10` stayed at or above `--job-pressure` for `--job-pressure-for` | `cgroup` |
| `domain_pressure` | The domain's `full avg10` stayed at or above `--domain-pressure` for `--domain-pressure-for` | both |
| `domain_floor` | The domain's free room fell below `--min-mem` | both |

oomwrap reads `memory.events` again after the main process exits, so a job that ended by `SIGKILL`
at its limit is reported as `job_limit` and not as the command's own exit.

Free swap is no stop condition. A full swap with free room in the domain is no danger, and low free
room already stops the job through `domain_floor`. Where the kernel has no PSI (before Linux 4.20,
or with PSI turned off), the pressure conditions are off and `doctor` says so.

The kernel counts the time it throttles a job above `memory.high` as memory stall of that job. A job
that stays far above `memory.high` can therefore meet `job_pressure`. That is intended:
`memory.high` gives a job room for a short peak, not a place to stay.

**Concurrent jobs.** Every guard of a domain reads the same domain numbers, so one low reading would
stop every guarded job at once. Before a stop for a domain reason, a guard waits and reads again,
and stops only when the condition still holds. The wait is shorter for a job that holds a larger
share of the domain's use, at most 2 s, so the largest job stops first and the others usually see
the pressure end. On a shared machine, guarded jobs still stop for memory that other users' work
takes: the guard protects the machine, not the job.

Preflight refuses a launch (exit 4) when `domain_floor` or `domain_pressure` already holds.

Memory that a device driver allocates for the job can sit outside these readings. On a machine with
unified GPU memory, a model's GPU allocations come from system RAM, and the memory controller may
not charge them to the job's cgroup. `domain_pressure` and `domain_floor` still see that memory on
a machine domain. Phase 6 measures whether the cgroup charges it, and step 12 of
[The end state](#the-end-state) counts it in the job.

### Machine-wide guard

oomwrap guards one job; a machine-wide guard stays the fallback for everything else. The inference
profiles require one. `earlyoom` counts when it runs. `systemd-oomd` counts only when it watches the
job: when a cgroup above the job has `ManagedOOMMemoryPressure=kill` or `ManagedOOMSwap=kill`,
because `systemd-oomd` monitors only those cgroups. The policy file can declare another guard by its
process name. `--require-machine-guard` and `--allow-no-machine-guard` replace `--require-earlyoom`
and `--allow-no-earlyoom`. In a container, a guard of the host is not visible; the inference
profiles then exit 3 unless `--allow-no-machine-guard` is given, and the message says why. oomwrap
never depends on `systemd-oomd` for its own decisions. When both guards act on the same cgroups,
`doctor` warns that two tools may kill for the same pressure.

### Stop sequence

`cgroup`: send `SIGTERM` to every PID in `cgroup.procs`, wait `--term-grace`, write `1` to
`cgroup.kill`, and wait until `cgroup.procs` is empty. `cgroup.kill` reaches every process in the
group, also one that left the process group. After `job_limit` or `outside_oom_kill`, oomwrap writes
`cgroup.kill` at once, because the job has already lost a process. systemd removes the scope
(`--collect`).

`poll`: the process-group sequence of 0.1.0.

**When the command ends.** When the job's main process exits, oomwrap ends the processes that remain
with the same sequence and returns the command's exit code. In `cgroup` this ends every process the
job started, also daemons that left the process group, such as build servers and compiler caches; in
`poll` such daemons survive, as in 0.1.0. A daemon meant to outlive the job must start outside
oomwrap.

**Job control.** When the job stops on `SIGTSTP` (Ctrl-Z), oomwrap takes the terminal back and
stops itself, so the shell sees a stopped job. On `SIGCONT`, oomwrap gives the terminal to the job
and continues it. 0.1.0 leaves the shell with a stopped job that it cannot resume.

**When oomwrap itself ends.** If oomwrap is killed with `SIGKILL`, the `cgroup` backend keeps the
kernel limits in force: the job goes on under `memory.max` and `memory.high`, without the pressure
stops. `doctor` and `oomwrap status` (step 9) list scopes without a guard by their lock. The `poll`
backend leaves the job unguarded, as 0.1.0 does.

### Exit codes

| Code | Meaning |
| --- | --- |
| The command's code | The command ended on its own |
| 2 | Invalid arguments or policy file, or a guard error after the launch: oomwrap could no longer read the domain, and ended the job first |
| 3 | oomwrap cannot guard as asked: a required machine-wide guard, the required backend or an explicit limit is not available. The event and the message name which |
| 4 | Preflight refused the launch: no room in the domain now |
| 126, 127 | The command could not be run, or was not found |
| 128 + n | oomwrap received signal n and ended the job |
| 137 | The guard stopped the job; the event names the reason |

A command killed by `SIGKILL` from elsewhere, for example from `earlyoom`, also ends with 137. The
event log is the record of why a job ended; the exit code is not.

### Events

Every event has a schema version, `backend`, the memory domain, and in the `cgroup` backend `unit`
and `cgroup`. `launched` holds the resolved limits in bytes. A stop event holds the `reason` from
the table above and the readings that decided it.

- **Where.** Without `--event-log`, events go to `${XDG_STATE_HOME:-~/.local/state}/oomwrap/`, one
  JSON-lines file per day, kept 14 days. `--event-log none` turns the log off. When the default log
  cannot be opened, oomwrap warns and runs the job; an explicit `--event-log` that cannot be opened
  stops the launch, as in 0.1.0.
- **What.** By default an event holds the program name only, not the full command line, because
  arguments can hold tokens and keys. `--log-command` adds the full command line.
- **How.** Each event is built in memory and written with one `write` to a file opened for append,
  so the events of concurrent jobs never mix inside a line.

`--record-readings <path>` writes every `Readings` value with the policy's decision as JSON lines.
This is the trace format of the replay tests.

### doctor and inspect

`doctor` reports:

- cgroup v2 and v1 mounts, the user manager, lingering, the delegated controllers, `cgroup.kill`,
  PSI, and the backend that `auto` would take and why;
- the memory domain, its size and free room, and the memory limits above the current shell;
- the resolved limits and the file or flag that set each one;
- the machine-wide guards, and whether `systemd-oomd` watches the job's cgroups;
- reserved huge pages and a ZFS ARC, which change what free room means;
- scopes that lost their guard;
- shims and wrappers that an earlier version made, which pass flags that 0.2.0 no longer has, with
  the command that writes them again.

### Configuration

Every call site passes its own floors today, so one machine's policy is copied into each script,
shim and wrapper. 0.2.0 reads one policy file: `/etc/oomwrap.toml`, then
`~/.config/oomwrap/config.toml`, each with a section for each profile. Flags override the file. A
value missing in all of them takes the defaults above.

Shims and wrappers carry no policy. Both are made by one generator and end in
`exec oomwrap run --shim <tool> -- "$real" "$@"`, so a later change of flags or defaults never
breaks an installed shim. `install-shims` and `wrap` rewrite a script that oomwrap made in place.
From 0.2.0 on, oomwrap records each wrapped path in its state folder, so `doctor` can find wrappers
outside `PATH`.

### Code structure

`src/lib.rs` (1,851 lines) splits by job, so that the guard core keeps no CLI, shim or file
system code (`AGENTS.md`):

| Module | Holds | IO |
| --- | --- | --- |
| `limits` | `Size` (bytes, share or `infinity`, with bounds), resolution against the domain, validation | none |
| `readings` | Parsers of `meminfo`, PSI, `memory.events`, `memory.stat`, `memory.current`, `mountinfo` and the process group field of `/proc/<pid>/stat` | none |
| `signal` | `ProcessId`, which holds only IDs greater than 1, and the only calls of `kill` and `killpg` | signals |
| `host` | `Host`: the proc root, the cgroup root, the `systemd-run` program, the page size and the machine-guard check, built once at the border; tests build a fake one | file reads |
| `domain` | Finding the memory domain and the caller's limits | file reads |
| `policy` | `Decision::evaluate(&StopLimits, &Readings, &mut PressureWindow)`, the preflight check and `StopReason` | none |
| `launch` | `exec-job` and the handshake | process |
| `backend` | `enum Backend { Cgroup(Scope), Poll(ProcessGroup) }`: launch, readings source, stop sequence | process and cgroup |
| `supervise` | The loop: signals, job control, terminal foreground, events, and one `stop(StopReason)` path | yes |
| `config` | The policy file, its merge with flags, and the profile table | file reads |
| `shims`, `events`, `doctor`, `cli` | The code of 0.1.0, moved | as today |

`Backend` is an enum, not a trait object: the set is closed at two.

kill(2) reads 0 as the caller's own process group and -1 as every process of the user, and
killpg(1) is kill(-1). A mutation test once turned a member list into `[-1]` and ended every
process of the user who ran it. So every signal takes a `ProcessId`, `clippy.toml` forbids `kill`
and `killpg` outside `signal`, and `.cargo/mutants.toml` keeps cargo-mutants out of the functions
that choose signal targets. This change came first, before phase 1, as its own pull request.

Smells of 0.1.0 that the split removes:

| Smell (place in 0.1.0) | Change |
| --- | --- |
| The child-exit block is copied twice in `run_guard`, and `kill_for_pressure`, `kill_for_parent_signal` and `cleanup_child_after_error` are near copies | One `stop(StopReason)` path; the loop returns one `Outcome` |
| Event names and labels are strings (`Event`), and exit codes are bare integers in five places | `enum EventKind`, and `enum Outcome` with one `exit_code()` |
| Sizes and durations stay `String` in the argument structs and are parsed inside `run_guard` | clap value parsers give `Size` and `Duration` at the border |
| Engine names sit in two separate lists (`detect_profile`, `known_high_memory_processes`) | One profile table: executables, Python modules and default policy |
| Test seams are environment variables read deep in the code (`OOMWRAP_MEMINFO_PATH`, `OOMWRAP_EARLYOOM_ACTIVE`) | One `Host` value passed in; the variables are read once, at the border |
| `parse_size` reads `GB` as GiB and computes through `f64` | Binary units `K`, `M`, `G` and `T` only, as systemd reads them, whole numbers, a percentage or `infinity`; `GB` is refused |
| Two near-copy bash generators bake the flags into every shim and wrapper | One generator, no policy in the script (above) |
| `serde_json::to_writer` on an unbuffered file writes one event in many pieces | One buffer, one `write` |

## Tests

- **Unit:** `Size` resolution, bounds and page rounding; the parsers; the memory domain over fake
  cgroup chains (none, a container limit, a limited service, an outer oomwrap scope); one table
  test per `StopReason`, with `job_limit` and `outside_oom_kill` told apart; the pressure window,
  which must not stop on one high reading; the stagger of domain stops; and the regression test of
  the false stop: full swap, free room, no pressure, no stop.
- **Integration, in CI:** `OOMWRAP_PROC_ROOT` and `OOMWRAP_CGROUP_ROOT`, read once into `Host`,
  point at fake trees; they replace `OOMWRAP_MEMINFO_PATH`. `Host` names the `systemd-run` program,
  and CI uses a fake one that runs only what follows `--`. The tests cover the handshake, `abort`, a
  launch that fails before the report, a failed exec, a signal during the handshake, a protocol
  version mismatch and the fallback to `poll`, and check in each case that the command ran at most
  once. Other tests change the fake pressure and event files while a bounded child runs and check
  the reason and the exit code; cover Ctrl-Z and resume in a pseudo-terminal; check that the
  default event log holds no arguments and that concurrent writers never mix lines; and read
  `/proc/<child>/oom_score_adj` and expect `1000`. The `poll` backend runs for real.
- **Replay of recorded runs, in CI:** traces from `--record-readings`, with a generic job label
  (`build`, `renderer`, `model load`) and no command line, host name or path, stored under
  `tests/fixtures/traces/` with the decision that each run must get. CI replays every trace through
  `policy`, so each change of the policy or of a default is checked against real history and not
  only against made-up numbers.
- **Real system, in CI on throwaway runners:** a helper binary under `tests/`, run on GitHub-hosted
  runners with lingering turned on for the runner user: Ubuntu 22.04 (systemd 249, below 253) and
  Ubuntu 24.04, each on `x86_64` and `aarch64`, and one container job for the `poll` backend and the
  cgroup domain. It
  - allocates past a `64M` `--memory-max`: `job_limit`, exit 137, no process left;
  - forks a child that calls `setsid()` and sleeps: the stop ends both;
  - runs at a `memory-high` far below its working set: `job_pressure`;
  - checks that the environment, the working folder, the exit code and a forwarded signal pass
    through `systemd-run` unchanged, that the control socket reaches `exec-job` but not the command,
    and that `/proc/<guard pid>/exe` runs after the oomwrap file was replaced;
  - runs inside a scope that has its own `MemoryMax`: `auto` takes `poll` with that cgroup as the
    domain.

  Only the job's own small scope runs out of memory; the runner itself never comes under pressure.
  Kernels before 5.14, cgroup v1 and hybrid hosts have no hosted runner and are covered only by the
  fake trees; the plan says so in `RELEASING.md`.
- **Machine pressure traces:** a separate workflow, started by hand, brings a throwaway runner
  virtual machine itself under memory pressure and records its readings as traces, for the replay
  tests and for the `domain_pressure` default. This is the only place where anything puts a whole
  machine under pressure, because the machine is discarded after the run.

Code that only the real-system suite can reach stays in thin adapters with a written reason for the
mutation-test skip, as 0.1.0 does today.

Mutation tests are optional. They are not part of CI, of a finished phase or of the release: a
phase is finished when the checks of `AGENTS.md` pass, with line coverage of at least 85%, and CI
is green. (The first version of this plan required a mutation run on CI for each phase. During
phase 2 the maintainer made them optional: the runs were slow, two of them ended canceled, and
the unit, integration and coverage checks already gate each change.) Mutation tests never run on
a workstation over code that sends signals, kills processes, writes `cgroup.kill` or calls
`systemd-run`: a mutant there can reach processes that the tests did not start. A requested run
uses the manual `Mutation tests` workflow, on a throwaway runner. Locally, only pure modules are
mutated, file by file. A test signals only processes that it started, and
creates and removes only its own `oomwrap-` scopes.

## Phases

Each phase is one pull request that passes the checks of `AGENTS.md`, and the main branch holds a
working tool after every phase. Users install the released crate, never the main branch: phase 1
changes the install command of the `memory-safe-launch` skill from the Git main branch to
`cargo install oomwrap --locked`, as the README already does first, and drops the README's
instruction to install the main branch. The skill and the README keep
describing the released version until phase 7 changes them together with the release, so no agent
reads 0.2.0 flags while it runs 0.1.0, or the reverse.

1. **Split `lib.rs`** into the modules above, with `Host`, the profile table, `EventKind`,
   `Outcome`, one `stop` path and typed `Size` and `Duration` arguments. `Size` keeps the parsing
   rules of 0.1.0 until phase 2. No change of behavior: the 41 integration tests pass unchanged,
   and the 10 unit tests move, with the same assertions, into the modules of the code they test.
   The flags of `install-shims` and `wrap` stay text until phase 5, because the scripts quote them
   as data and a test checks that a hostile value stays inert. The skill's install command changes
   as above.
2. **Policy core.** The memory domain, `Size` with shares, bounds and `infinity`, the parsers, the
   policy and its tests. The `poll` backend moves to the new policy: no swap floor, domain pressure
   and floor, shares of the domain. The machine-wide guard check. The exit-code table. CLI cutover:
   `--min-swap`, `--require-earlyoom` and `--allow-no-earlyoom` go, and an unknown flag before the
   command is an error instead of the start of the command, so an old flag never runs as a
   program. The test seam `OOMWRAP_EARLYOOM_ACTIVE` becomes `OOMWRAP_MACHINE_GUARD_ACTIVE`. Until
   phase 5 replaces them, shims and wrappers pass `--min-mem` only when `install-shims` or `wrap`
   got one, and `OOMWRAP_ALLOW_NO_MACHINE_GUARD=1` passes `--allow-no-machine-guard`. The
   `--job-pressure` flags come with the `cgroup` backend in phase 4, because only that backend
   reads the job's own pressure.
3. **Launch and supervision.** `exec-job` with the handshake and the score of `1000`, job control,
   the event log changes, the schema version and `--record-readings`.
4. **The `cgroup` backend:** the four checks, the scope launch through `/proc/<guard pid>/exe`, the
   cgroup check in the handshake, page-rounded sizes, the launch-time cap of `--memory-max`,
   readings, `job_limit` and `outside_oom_kill`, the stop sequence, the cleanup at the command's
   end, the lock, the stagger of domain stops, explicit limits as a requirement, `--backend`, and
   root on the system manager. `--use-systemd-scope` goes.
5. **Configuration, `doctor`, `inspect`, shims and wrappers:** the policy file, the report above,
   the one generator of shims and wrappers without policy, rewriting in place, and the record of
   wrapped paths.
6. **Real-system suite, traces and calibration:** the CI runner matrix, the machine pressure
   workflow, recorded job traces of a build, a renderer and a model load, the pressure defaults set
   from them, and a model load on a machine with unified GPU memory to measure whether the cgroup
   charges the GPU's allocations. The results go into this document.
7. **Documents and release:** `README.md`, the `memory-safe-launch` skill (its flags, floors and
   recovery steps), `implementation-plan.md` (its design section is replaced,
   and its rule that CI never induces an out-of-memory condition is narrowed to the rule above),
   `CHANGELOG.md` with the upgrade steps, version 0.2.0. The release workflow also builds static
   binaries (`x86_64` and `aarch64`, musl), runs the test suite against those binaries, and attaches
   them to the GitHub release with SHA-256 checksums and build provenance attestations. `cargo
   install` stays.

## The end state

0.2.0 guards one job well. The end state guards a user's whole set of heavy jobs: each job holds a
memory contract that the kernel enforces, against one policy, with nothing to tune per machine and
every outcome explained. Each step below is its own release after 0.2.0.

8. **One shared budget.** Every guarded scope of a user starts under one slice of that user's
   systemd manager, `oomwrap.slice`, with its own `MemoryMax` and `MemoryHigh` from the policy file.
   The kernel then enforces the total of that user's guarded jobs together, whichever script starts
   them. A budget across users needs a system slice that root sets up, and stays out of scope.
9. **Wait for room instead of refusing.** A job may declare its need (`--expect 12G`, or a profile
   default). With `--wait-for-room`, a job whose need does not fit the slice and the domain waits,
   in order of arrival, until it fits, instead of failing at preflight (exit 4). This replaces
   hand-made queues that cap heavy jobs at a fixed count of slots. `oomwrap status` lists the jobs
   in the slice, the ones that wait, and scopes without a guard. `oomwrap status --json` gives the
   budget, its use, the free room and the queue in a stable form, so a script or an agent can plan
   a launch before it starts one, instead of learning the limits from a refusal.
10. **Event-driven supervision.** One `poll()` loop on the child's `pidfd`, a `signalfd`, PSI
    triggers, `cgroup.events` (the job is empty) and `memory.events` (the kernel killed). An
    unprivileged trigger's window is at most 10 s and a multiple of 2 s, so a trigger only wakes
    oomwrap, which then times the rest of `--job-pressure-for` itself. Where unprivileged triggers
    are not allowed, oomwrap reads `avg10` as before.
11. **A documented event schema.** The schema of the event log is documented and kept stable, and
    `oomwrap explain` turns a log into the story of each stop: reason, readings and limits.
12. **Count device memory that comes from system RAM.** When phase 6 shows that the cgroup does not
    charge a unified-memory GPU's allocations, oomwrap adds them to the job's readings and a new
    reason, `device_memory`, stops the job when the sum passes `--memory-max`. Sources, in order:
    the kernel's `dmem` controller (`dmem.current`) where the driver registers its regions, then the
    DRM per-file memory statistics in `/proc/<pid>/fdinfo`, then a vendor library such as NVML only
    where the driver offers neither. Memory of a discrete GPU is not system RAM and never counts
    against `--memory-max`.

What stays out:

- **Handing pressure stops to `systemd-oomd`.** Many machines do not install it, and where it runs
  it watches only the cgroups that ask for it.
- **Freezing a job instead of stopping it.** A frozen job keeps its memory, so freezing does not
  relieve the pressure that caused the stop.
- **A Rust library API.** It adds public surface to keep stable, and a process cannot easily move
  itself into a new cgroup; the command line is the contract.
- **Compatibility flags for 0.1.0.** The cutover is hard: the old flags fail with an error, and the
  changelog and `doctor` give the upgrade steps.
- **A third backend that writes a delegated cgroup subtree directly**, for example inside a rootless
  container with its own cgroup namespace and no systemd. `poll` with the container's limit as the
  domain already protects the container, and a third backend adds one more launch and stop path to
  test. It comes back only when a user needs per-job kernel limits there.
- **macOS and systems without Linux cgroups or PSI.** macOS stays out of scope, as in 0.1.0. A Linux
  system without systemd uses `poll`.

## Risks and open questions

- **Memory outside the cgroup's count.** A device driver can allocate memory for the job that the
  memory controller does not charge, for example unified GPU memory. The domain conditions stay for
  that reason; phase 6 checks a model load on such a machine, and step 12 counts that memory.
- **Concurrent jobs before step 8.** Each job's cap uses the free room at its own launch, so jobs
  that start together can each claim the same room. Only the domain conditions catch the sum.
- **A job that lowers its own score.** An unprivileged process can lower its `oom_score_adj` again,
  down to the lowest value a privileged process set for it. A job that does so is no longer the
  first victim of a machine-wide kill; its own limits still hold.
- **The control socket through `systemd-run`.** The handshake needs `systemd-run` to pass an
  inherited descriptor to the program it executes. A probe confirmed it on systemd 255; the CI
  runner matrix checks it on systemd 249 and 255.
- **Launch cost.** `systemd-run` adds a bus round trip to each launch. Phase 6 measures it, and the
  default shim list stays limited to long-running tools.
- **Domain pressure default.** `20%` for `10s` is provisional until the machine pressure traces of
  phase 6.
- **Free room in a cgroup domain.** `memory.max − memory.current + inactive_file` approximates
  `MemAvailable`; phase 6 compares it with the kernel's own reclaim in the container job.
- **Breaking change.** Scripts and 0.1.0 shims and wrappers that pass `--min-swap` fail with an
  error after the upgrade. The changelog lists the upgrade steps: install the shims again, run
  `oomwrap wrap` again on each wrapped path.
