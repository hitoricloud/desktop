#!/usr/bin/env bash
# Package version: the tag without its "v", else Cargo.toml's version and the short commit.
set -euo pipefail
if [[ "${GITHUB_REF_TYPE:-}" == tag ]]; then
  echo "${GITHUB_REF_NAME#v}"
  exit
fi
root="$(cd "$(dirname "$0")/.." && pwd)"
sha="${GITHUB_SHA:-$(git -C "$root" rev-parse HEAD)}"
echo "$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)-${sha:0:8}"
