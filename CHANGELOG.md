# Changelog

## 0.2.0

0.2.0 replaces the guard of 0.1.0. Each job now runs in its own cgroup scope
with kernel memory limits, and oomwrap stops it on the memory pressure that the
kernel reports, so it no longer needs fixed floors for each machine. This is a
breaking release. Old flags fail with an error, and 0.1.0 shims and wrappers
must be written again (see [Upgrade from 0.1.0](#upgrade-from-010)).

### Added

- A `cgroup` backend. With `--backend auto` (the default), oomwrap runs the job
  in a transient scope of the systemd manager with `memory.max` and
  `memory.high`. A stop writes `cgroup.kill`, which also ends children that left
  the process group with `setsid()`. Where the scope cannot run, such as in
  a container or a CI job, oomwrap uses the `poll` backend.
- The memory domain. oomwrap reads the size, free room and pressure of the
  memory that the job competes for: the machine, or the smallest limited cgroup
  above the caller, such as a container. Every share is a share of that domain.
- Defaults that follow the machine: `--memory-max` is the smallest of `85%`, the
  free room at launch minus `--min-mem`, and any lower limit above the job;
  `--memory-high` is `90%` of it; `--min-mem` is `15%`, at least `256M`, at most
  `16G`. A size can be a share (`85%`) or `infinity`.
- Pressure stops from the kernel's PSI figures: `--job-pressure` (`60%` for
  `30s`) and `--domain-pressure` (`10%` for `10s`, only while the domain has
  less than a quarter of its memory free). The defaults come from recorded runs
  on real machines.
- Stop reasons in the events: `job_limit`, `outside_oom_kill`, `job_pressure`,
  `domain_floor` and `domain_pressure`.
- A policy file: `/etc/oomwrap.toml`, then
  `${XDG_CONFIG_HOME:-~/.config}/oomwrap/config.toml`, with a section for each
  profile. A flag wins over the user file, which wins over the system file.
- `--require-machine-guard` and `--allow-no-machine-guard`. Besides `earlyoom`,
  `systemd-oomd` counts as a machine-wide guard when it watches the job. The
  policy file can name more guards in `machine-guards`.
- `--memory-swap-max`, `--backend`, `--log-command`, `--record-readings`,
  `--shim`, and `doctor --profile <name> [--json]`.
- An event log by default, one file per day under
  `${XDG_STATE_HOME:-~/.local/state}/oomwrap/`, kept for 14 days.
  `--event-log none` turns it off. Each event has a `schema_version`.
- `doctor` reports the backend that `auto` takes and why, the memory domain and
  each setting with its source. It also finds scopes that lost their guard and
  outdated shims and wrappers, and prints the command that repairs each.
- Job control: Ctrl-Z, `fg` and `bg` work on a guarded job.
- The job's `oom_score_adj` is `1000`, so a machine-wide killer picks the job
  first.
- Static musl binaries for `x86_64` and `aarch64` on each GitHub release, with
  SHA-256 checksums and build provenance attestations.

### Changed

- When the command ends, oomwrap ends the processes that it left behind. With
  the `cgroup` backend this includes daemons that left the process group, such
  as build servers and compiler caches. Start a daemon that must outlive the
  job outside oomwrap.
- An unknown flag before the command is an error. 0.1.0 took it as the start of
  the command.
- `--memory-max` and `--memory-high` set the kernel limits of the job's scope.
  An explicit limit needs the `cgroup` backend. Without that backend, oomwrap
  exits 3 and runs nothing.
- Exit 3 now also means that the required backend or an explicit limit is not
  available, and exit 4 that the domain has no room for the job now.
- Shims and wrappers carry no policy. They call `oomwrap run --shim <tool>`, and
  the policy file and the defaults decide the limits.
- The events changed: `stopped` with a `reason` replaces `memory_pressure_kill`,
  `interrupted` replaces `parent_signal_kill`, `exited` replaces `child_exit`,
  and `cleanup` replaces `process_group_cleanup`. Events hold the program name
  only, without its arguments, unless `--log-command` is given.
- `inspect --json` holds the whole `doctor` report in place of its `default_*`
  fields.

### Removed

- `--min-swap` and the swap floor. Linux moves idle pages to swap while much
  memory stays free, so the swap floor stopped healthy jobs. Low free room and
  pressure now stop the job.
- `--require-earlyoom` and `--allow-no-earlyoom`, replaced by
  `--require-machine-guard` and `--allow-no-machine-guard`.
- `--use-systemd-scope`, replaced by `--backend cgroup`.
- The `--min-mem` and `--min-swap` flags of `install-shims` and `wrap`, and the
  variables `OOMWRAP_MIN_MEM`, `OOMWRAP_MIN_SWAP`, `OOMWRAP_PROFILE`,
  `OOMWRAP_ALLOW_NO_EARLYOOM` and `OOMWRAP_EVENT_LOG` of the scripts. Put these
  values in the policy file. `OOMWRAP_BIN` and `OOMWRAP_REAL_<TOOL>` stay.

### Upgrade from 0.1.0

1. Install 0.2.0 with `cargo install oomwrap --locked`, or with the binary of
   the release.
2. Run `oomwrap doctor` and do the repairs that it names. For the `cgroup`
   backend, turn on lingering with `loginctl enable-linger`.
3. Remove `--min-swap` from scripts and commands. Replace `--require-earlyoom`
   with `--require-machine-guard`, `--allow-no-earlyoom` with
   `--allow-no-machine-guard`, and `--use-systemd-scope` with
   `--backend cgroup`.
4. Check each `--min-mem`. A fixed value such as `24G` still works, but the
   default now follows the machine. Remove it, or move it into the policy file.
5. Move the values that shims and wrappers took from `OOMWRAP_MIN_MEM`,
   `OOMWRAP_MIN_SWAP`, `OOMWRAP_PROFILE`, `OOMWRAP_ALLOW_NO_EARLYOOM` and
   `OOMWRAP_EVENT_LOG` into the policy file, for example:

   ```toml
   min-mem = "8G"

   [profile.vllm]
   require-machine-guard = false
   ```

6. Write the shims again with `oomwrap install-shims`, with the `--bin-dir` and
   `--tool` values that you used before. Then run `oomwrap wrap <path>` again
   for each wrapped program. Both commands rewrite the 0.1.0 scripts in place.
   Until then, the 0.1.0 scripts fail with exit 2.
7. Update tools that read the event log to the new event names and fields.
8. Run `oomwrap doctor` again. It must name no outdated shim or wrapper.

## 0.1.0

- Add process-scoped RAM and swap protection for local commands.
- Add inference profiles, event logs, PATH shims, and absolute command wrappers.
- Add the embedded `memory-safe-launch` agent skill through Skillflag.
