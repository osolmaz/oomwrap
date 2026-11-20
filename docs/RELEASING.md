# Releasing oomwrap

The crate name `oomwrap` was available on crates.io when this release process
was prepared. Check it again before the first release because availability can
change.

## One-time setup

1. Create a crates.io API token that can publish `oomwrap`.
2. Save it as the GitHub Actions secret `CARGO_REGISTRY_TOKEN` in
   `osolmaz/oomwrap`.
3. Keep the repository default branch set to `main`.

The workflow uses the token only through `cargo publish`. Do not add the token
to the repository or release files.

## Prepare a release

1. Select the next version with Semantic Versioning.
2. Update `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`.
3. Run:

   ```bash
   cargo fmt --check
   cargo check --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace --all-targets
   slophammer-rs check .
   cargo package --list
   cargo publish --dry-run --locked
   ```

4. Inspect the packaged crate under `target/package/` and confirm that the
   embedded `memory-safe-launch` skill works from the packaged source.

   Mutation tests are optional and are not part of a release. The `mutants`
   job of CI runs only on a manual dispatch with the `mutants` input, for a
   run that the maintainer asks for.
5. Merge or push the release commit to `main` and wait for the `CI` and
   `Real system` workflows to pass.

## Real-system checks

The `Real system` workflow runs `tests/real_system.rs` on throwaway
GitHub-hosted runners: Ubuntu 22.04 (systemd 249) and Ubuntu 24.04 (systemd
255), each on `x86_64` and `aarch64`, and a container with a memory limit and
no service manager. Its jobs run out of memory on purpose, each in a small
scope of its own. Kernels before Linux 5.14, cgroup v1 and hybrid hosts have no
hosted runner; only the fake trees of `tests/cli.rs` cover them.

The `Pressure traces` workflow, started by hand, records traces and brings one
runner itself under memory pressure. Review its traces before they go into
`tests/fixtures/traces`, and add each one with its decision to `traces.toml`.

## Publish

Create and publish a GitHub Release from the exact `main` commit, with a tag
that matches the Cargo version, such as `v0.1.0`.

The `Publish crate` workflow then:

- checks that the tag matches `Cargo.toml`;
- checks that the tagged commit is on `main`;
- refuses to replace an existing crates.io version;
- runs the Rust checks and package verification;
- publishes with `cargo publish --locked`.

Do not create a tag or GitHub Release until publication is explicitly
authorized.
