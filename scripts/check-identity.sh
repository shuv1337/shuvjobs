#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

fail() {
  printf 'identity check failed: %s\n' "$1" >&2
  exit 1
}

grep -Fq 'repository = "https://github.com/shuv1337/shuvjobs"' Cargo.toml ||
  fail "canonical repository metadata is missing"
grep -Fqx '    "crates/shuvjobs-core",' Cargo.toml ||
  fail "shuvjobs-core workspace member is missing"
grep -Fqx '    "crates/shuvjobs-adapters",' Cargo.toml ||
  fail "shuvjobs-adapters workspace member is missing"
grep -Fqx '    "crates/shuvjobs-tui",' Cargo.toml ||
  fail "shuvjobs-tui workspace member is missing"
grep -Fqx '    "crates/shuvjobs",' Cargo.toml ||
  fail "shuvjobs CLI workspace member is missing"
package_name=$(sed -n '/^\[package\]/,/^\[/s/^name = "\([^"]*\)"/\1/p' crates/shuvjobs/Cargo.toml)
bin_name=$(sed -n '/^\[\[bin\]\]/,/^\[/s/^name = "\([^"]*\)"/\1/p' crates/shuvjobs/Cargo.toml)
test "$package_name" = shuvjobs || fail "canonical package name is missing"
test "$bin_name" = shuvjobs || fail "canonical binary name is missing"
grep -Fq 'ShuvJobs —' crates/shuvjobs-tui/src/lib.rs ||
  fail "canonical display name is missing"
grep -Fq 'asset: shuvjobs-linux-x86_64' .github/workflows/release.yml ||
  fail "canonical release assets are missing"
grep -Fq -- '-p shuvjobs' .github/workflows/release.yml ||
  fail "release workflow does not build the canonical package"
grep -Fq 'Copyright (c) 2026 Ali Goren' LICENSE ||
  fail "upstream copyright notice is missing"
grep -Fq 'https://github.com/aligoren/sta' docs/FORK.md ||
  fail "upstream provenance is missing"

for crate in shuvjobs-core shuvjobs-adapters shuvjobs-tui shuvjobs; do
  grep -Fqx 'license.workspace = true' "crates/$crate/Cargo.toml" ||
    fail "crates/$crate does not inherit the workspace license"
  cmp -s LICENSE "crates/$crate/LICENSE" ||
    fail "crates/$crate/LICENSE does not match the root LICENSE"
done

if matches=$(find . -type f \
  ! -path './target/*' \
  ! -path './.git/*' \
  ! -path './.jj/*' \
  ! -path './docs/FORK.md' \
  ! -path './scripts/check-identity.sh' \
  -exec grep -nIE '(^|[^[:alnum:]_])(sta|STA)([^[:alnum:]_]|$)|sta[-_]|github\.com/(ali|aligoren)/shuvjobs' {} + 2>/dev/null); then
  printf '%s\n' "$matches" >&2
  fail "retired branding remains on canonical surfaces"
fi

printf 'ShuvJobs identity boundary is intact.\n'
