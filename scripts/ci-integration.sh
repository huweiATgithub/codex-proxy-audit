#!/usr/bin/env bash
set -euo pipefail

: "${HOST_VERSION:?HOST_VERSION is required}"
: "${HOST_REVISION:?HOST_REVISION is required}"
: "${PLUGIN_ARCHIVE:?PLUGIN_ARCHIVE is required}"
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
[[ "$HOST_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ "$HOST_REVISION" =~ ^[0-9a-f]{40}$ ]]
[[ -f "$PLUGIN_ARCHIVE" ]]

upstream_revision=$(git ls-remote --exit-code https://github.com/zyycn/codex-proxy-rs.git \
  "refs/tags/v${HOST_VERSION}" "refs/tags/v${HOST_VERSION}^{}" | \
  awk '$2 ~ /\^\{\}$/ { peeled = $1 } $2 !~ /\^\{\}$/ { tag = $1 } END { print peeled ? peeled : tag }')
if [[ "$upstream_revision" != "$HOST_REVISION" ]]; then
  echo "Host v${HOST_VERSION} resolves to ${upstream_revision}, expected ${HOST_REVISION}" >&2
  exit 1
fi

download_dir="$RUNNER_TEMP/host-release"
host_archive="codex-proxy-rs_${HOST_VERSION}_linux_amd64.tar.gz"
mkdir -p "$download_dir"
gh release download "v${HOST_VERSION}" --repo zyycn/codex-proxy-rs \
  --pattern "$host_archive" --pattern checksums.txt --dir "$download_dir" --clobber
awk -v name="$host_archive" '$2 == name { print; found++ } END { if (found != 1) exit 1 }' \
  "$download_dir/checksums.txt" > "$download_dir/host-checksum.txt"
(cd "$download_dir" && sha256sum --check --strict host-checksum.txt)
tar -xzf "$download_dir/$host_archive" -C "$download_dir"

export TMPDIR="$RUNNER_TEMP/integration"
mkdir -p "$TMPDIR" dist/verification
collect_evidence() {
  local status=$?
  trap - EXIT
  shopt -s nullglob
  local evidence
  for evidence in "$TMPDIR"/codex-audit-integration-*/result.json "$TMPDIR"/codex-audit-integration-*/*.png; do
    cp -- "$evidence" dist/verification/ || echo "Unable to copy integration evidence: $evidence" >&2
  done
  exit "$status"
}
trap collect_evidence EXIT

node scripts/integration.js \
  --host-binary "$download_dir/codex-proxy-rs/codex-proxy-rs" \
  --host-web "$download_dir/codex-proxy-rs/web/dist" \
  --package "$PLUGIN_ARCHIVE" \
  --host-commit "$HOST_REVISION" \
  --next-wait-ms 125000
