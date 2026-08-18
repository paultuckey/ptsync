# Release

Releasing is manual and local, driven by [`cargo release`](https://github.com/crate-ci/cargo-release):

```shell
cargo install cargo-release
```

One command does the whole sequence — bump the version, rewrite the changelog, commit, tag, publish
to crates.io, and push. The settings live in [`release.toml`](../release.toml).

`cargo publish` is irreversible — a version can be yanked, never replaced — so run the dry run and
the checks first.

## 1. Write the changelog

Add entries under `## [Unreleased]` in [`CHANGELOG.md`](../CHANGELOG.md). The release turns that
heading into the version being released, dates it, and re-points the links at the bottom; nothing
there needs editing by hand.

This section is the release notes. [`release.yml`](../.github/workflows/release.yml) copies it onto
the GitHub release verbatim and fails the build if the version has no section, so an empty
`[Unreleased]` is worth filling in before step 4 rather than after.

## 2. Run the checks

```shell
cargo fmt --check && cargo clippy --tests && cargo test
```

`cargo test` fails when `docs/cli.md` or `docs/db-schema.md` are stale, and when the console output
no longer matches `tests/snapshots/sync.txt`. Regenerate the docs with `UPDATE_DOCS=1 cargo test`,
and if the snapshot changed, re-record the demo GIF — see
[Development.md](../Development.md#demo-gif). The GIF is linked into the README by absolute URL, so
it is the version on `main` that readers see, including on the crates.io page.

Check what will actually be uploaded — this is where a stray database or scratch file in the
working tree shows up:

```shell
cargo package --list
```

## 3. Dry run

`major`, `minor` or `patch` — or an exact version. Without `--execute` nothing is written,
committed, pushed or uploaded:

```shell
cargo release minor
```

Read the diff it prints for the changelog and the manifest.

## 4. Release

Needs a crates.io token — `cargo login` once, or `CARGO_REGISTRY_TOKEN` in the environment. The
account also needs a verified email address; crates.io rejects the upload without one. The same
level as the dry run, plus `--execute`:

```shell
cargo release minor --execute
```

It asks for confirmation before anything irreversible.

That is the last manual step. Publishing happens before the tag is pushed, so once the tag lands on
GitHub the crate is already up, and pushing it triggers
[`release.yml`](../.github/workflows/release.yml), which creates the GitHub release with the
changelog section for that version as its body. Nothing is attached to it — `cargo install ptsync`
is the install path — so the release exists to notify watchers and to give the version notes.

If that workflow fails, the release can be written by hand against the tag on
[GitHub](https://github.com/paultuckey/ptsync/releases/new); it is a mirror of the changelog, and
nothing about the published crate depends on it.

## The first release

`ptsync` has never been published, and `Cargo.toml` already reads `0.1.0` — so the first release
publishes that version rather than bumping past it:

```shell
cargo release --unpublished --execute
```

`--unpublished` selects the crate because its current version is not on crates.io, which is exactly
true once and never again. Every release after this one names a level, as above.

Two things only the first release needs:

- **Claim the name.** `ptsync` is unregistered, and crates.io hands it to whoever publishes first.
  Nothing reserves it in the meantime.
- **Point the repo at the crate.** Set the GitHub repository's homepage field to
  `https://crates.io/crates/ptsync`, and change the README's install line if it still says
  `--git`.
