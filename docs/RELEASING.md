# Releasing oomwrap

A release publishes the crate to crates.io and attaches static Linux binaries
to the GitHub release. The `Release` workflow does both after a GitHub release
is published.

## One-time setup

The crate publishes with crates.io trusted publishing, so no API token is
stored anywhere. In the crate settings on crates.io, a trusted publisher names
the GitHub repository `osolmaz/oomwrap` and the workflow file `publish.yml`.
The `crates-io` job of that workflow asks crates.io for a token that lasts 30
minutes, publishes with it, and revokes it at the end of the job. A rename of
`publish.yml` needs the same change in the crates.io settings.

Keep the repository default branch set to `main`.

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

   A pull request that changes `Cargo.toml`, `Cargo.lock`, `publish.yml` or
   `.github/scripts` also runs the build and test jobs of the `Release`
   workflow, so the musl binaries are tested before the release.

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
that matches the Cargo version, such as `v0.2.0`. Paste the version's section
of `CHANGELOG.md` into the release notes.

The `Release` workflow then:

- checks that the tag matches `Cargo.toml`, that the tagged commit is on
  `main`, and that crates.io has no such version;
- builds `oomwrap` for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`, and checks that both are static;
- runs `cargo test --workspace --all-targets` against each binary through
  `OOMWRAP_TEST_BIN`, and checks that the tests left the file unchanged;
- runs the real-system suite against the binaries on Ubuntu 22.04 and 24.04 of
  both architectures, and in a container;
- attaches `oomwrap-<version>-<arch>-linux-musl` and `SHA256SUMS` to the
  release, with a build provenance attestation for each binary;
- publishes the crate with `cargo publish --locked` and a short-lived token
  from trusted publishing.

When a job fails, nothing is published to crates.io. Fix the cause, delete the
release and its tag, and publish the release again.

Do not create a tag or GitHub Release until publication is explicitly
authorized.
