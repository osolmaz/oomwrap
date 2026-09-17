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
5. Merge or push the release commit to `main` and wait for CI to pass.

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
