# Releasing

Releases follow semantic versioning and are tagged `v<MAJOR>.<MINOR>.<PATCH>`
(for example `v1.0.0`, `v1.1.0`, `v1.1.1`).

Pushing a tag matching `v*.*.*` triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which
builds binaries for all four supported targets and uploads them to a
new GitHub Release.

## Cutting a release

1. Update `workspace.package.version` and the three internal dependency
   versions in the root `Cargo.toml`, then verify and commit:

   ```sh
   cargo test --workspace --locked
   cargo package --workspace --locked --no-verify
   jj commit -m "release: v1.2.0"
   ```

2. Point `main` and the release tag at the release commit, then push them:

   ```sh
   jj bookmark set main -r @-
   jj tag set v1.2.0 -r @-
   jj git push --bookmark main
   git push origin v1.2.0
   ```

The workflow takes a few minutes. When it finishes, the release
appears under [Releases](https://github.com/shuv1337/shuvjobs/releases)
with these assets:

- `shuvjobs-linux-x86_64`
- `shuvjobs-linux-aarch64`
- `shuvjobs-macos-x86_64`
- `shuvjobs-macos-aarch64`
