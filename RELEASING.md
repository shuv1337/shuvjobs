# Releasing

Releases follow semantic versioning and are tagged `v<MAJOR>.<MINOR>.<PATCH>`
(for example `v1.0.0`, `v1.1.0`, `v1.1.1`).

Pushing a tag matching `v*.*.*` triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which
builds binaries for all four supported targets and uploads them to a
new GitHub Release.

## Cutting a release

1. Bump the version in every workspace `Cargo.toml` and commit:

   ```sh
   git commit -am "Release v1.2.0"
   ```

2. Tag and push:

   ```sh
   git tag v1.2.0
   git push origin main
   git push origin v1.2.0
   ```

The workflow takes a few minutes. When it finishes, the release
appears under [Releases](https://github.com/aligoren/sta/releases)
with these assets:

- `sta-linux-x86_64`
- `sta-linux-aarch64`
- `sta-macos-x86_64`
- `sta-macos-aarch64`
