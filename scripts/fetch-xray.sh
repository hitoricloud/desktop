#!/usr/bin/env bash
# Installs the prebuilt Xray core from the private Xray-core repository's CI into bin/xray.
# The source stays private: only the `build` job's checked artifact (dist/xray-linux-amd64 + SHA256SUMS) is used.
#
#   scripts/fetch-xray.sh [ref]        newest successful build on a branch or tag (default: main)
#   XRAY_JOB=<id> scripts/fetch-xray.sh one exact CI job
#
# Auth: $GITLAB_TOKEN (personal/project token with read_api), $CI_JOB_TOKEN in CI, else `glab` (glab auth login).
set -euo pipefail

project="${XRAY_PROJECT:-hitoricloud/vpn/Xray-core}"
ref="${1:-main}"
api="projects/${project//\//%2F}"
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/bin"

# get <api path> [output file]
if [[ -n "${GITLAB_TOKEN:-}${CI_JOB_TOKEN:-}" ]]; then
  header="PRIVATE-TOKEN: ${GITLAB_TOKEN:-}"
  [[ -n "${GITLAB_TOKEN:-}" ]] || header="JOB-TOKEN: $CI_JOB_TOKEN"
  get() { curl -fsSL --retry 2 -H "$header" -o "${2:-/dev/stdout}" "${GITLAB_API:-https://gitlab.com/api/v4}/$1"; }
elif command -v glab >/dev/null; then
  get() { glab api "$1" >"${2:-/dev/stdout}"; }
else
  echo "fetch-xray: set GITLAB_TOKEN or install glab and run 'glab auth login'" >&2
  exit 1
fi

if [[ -n "${XRAY_JOB:-}" ]]; then
  job="$(get "$api/jobs/$XRAY_JOB")"
else
  # Successful jobs, newest first; the build job only runs after verify passed.
  job="$(get "$api/jobs?scope[]=success&per_page=100" | jq --arg ref "$ref" '[.[] | select(.name == "build" and .ref == $ref and (.artifacts_file != null))][0]')"
fi
id="$(jq -r '.id // empty' <<<"$job")"
[[ -n "$id" && "$(jq -r .status <<<"$job")" == success ]] || {
  echo "fetch-xray: no successful build job with artifacts for '$ref' in $project (all pipelines failing, or artifacts expired)" >&2
  exit 1
}
sha="$(jq -r .commit.id <<<"$job")"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
# SHA256SUMS names the file as CI built it: dist/xray-linux-amd64.
mkdir "$tmp/dist"
get "$api/jobs/$id/artifacts/dist/xray-linux-amd64" "$tmp/dist/xray-linux-amd64"
get "$api/jobs/$id/artifacts/dist/SHA256SUMS" "$tmp/dist/SHA256SUMS"
(cd "$tmp" && sha256sum --check --strict --quiet dist/SHA256SUMS)
chmod 755 "$tmp/dist/xray-linux-amd64"
version="$("$tmp/dist/xray-linux-amd64" version | head -n1)"

mkdir -p "$out"
install -m 755 "$tmp/dist/xray-linux-amd64" "$out/xray"
jq -n --arg project "$project" --arg ref "$ref" --argjson job "$id" --arg commit "$sha" --arg version "$version" \
  --arg sha256 "$(cut -d' ' -f1 "$tmp/dist/SHA256SUMS")" --arg fetched "$(date -u +%FT%TZ)" \
  '{project: $project, ref: $ref, job: $job, commit: $commit, version: $version, sha256: $sha256, fetched: $fetched}' >"$out/xray.json"
echo "fetch-xray: $version (job $id, ${sha:0:8}) -> bin/xray"
