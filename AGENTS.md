# AGENTS.md

- Use the Slophammer Rust standards from `osolmaz/slophammer/docs/AGENT_ENTRYPOINT.md`.
- Keep the guard core independent from CLI formatting, shell shim generation, and filesystem IO when practical.
- Do not add unsafe Rust unless there is no safe OS API for the operation and the reason is documented in code.
- Do not weaken memory guards or tests to make local checks pass.
- Run these commands before finishing code changes:

```bash
cargo fmt --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Run `slophammer-rs check .` when `slophammer-rs` is available locally.

## Signals and mutation tests

- Send every signal through `src/signal.rs`. Its `ProcessId` holds only IDs
  greater than 1, because kill(2) reads 0 as the caller's own process group and
  -1 as every process of the user, and killpg(1) is kill(-1). `clippy.toml`
  forbids `kill` and `killpg` everywhere else.
- Mutation tests are optional. They are not part of the checks above, of a
  finished change, or of a release, and CI does not run them. Do not run them
  unless the maintainer asks for a run.
- Never run `cargo mutants` on a workstation over code that sends signals,
  kills processes, writes `cgroup.kill`, or calls `systemd-run` or `systemctl`.
  A mutant of such code can signal processes that the tests did not start; one
  mutant that returned the process ID -1 sent SIGTERM to every process of the
  user. `.cargo/mutants.toml` excludes the functions that choose signal
  targets. A requested run uses the `mutants` job of `CI` (a manual dispatch
  with the `mutants` input), on a throwaway runner; locally, mutate only pure
  modules with `--file`.
- A test signals only processes that it started itself, and creates and
  removes only its own transient scopes with the `oomwrap-` prefix.
