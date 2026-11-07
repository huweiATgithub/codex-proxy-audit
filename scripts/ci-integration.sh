#!/usr/bin/env bash
set -euo pipefail

: "${HOST_VERSION:?HOST_VERSION is required}"
: "${HOST_REVISION:?HOST_REVISION is required}"
: "${EXPECT_OPEN_EXTERNAL:?EXPECT_OPEN_EXTERNAL is required}"
: "${PLUGIN_ARCHIVE:?PLUGIN_ARCHIVE is required}"
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
[[ "$HOST_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ "$HOST_REVISION" =~ ^[0-9a-f]{40}$ ]]
[[ "$EXPECT_OPEN_EXTERNAL" == true || "$EXPECT_OPEN_EXTERNAL" == false ]]
[[ -f "$PLUGIN_ARCHIVE" ]]

upstream_revision=$(git ls-remote --exit-code https://github.com/zyycn/codex-proxy-rs.git \
  "refs/tags/v${HOST_VERSION}" "refs/tags/v${HOST_VERSION}^{}" | \
  awk '$2 ~ /\^\{\}$/ { peeled = $1 } $2 !~ /\^\{\}$/ { tag = $1 } END { print peeled ? peeled : tag }')
if [[ "$upstream_revision" != "$HOST_REVISION" ]]; then
  echo "Host v${HOST_VERSION} resolves to ${upstream_revision}, expected ${HOST_REVISION}" >&2
  exit 1
fi

download_dir="$RUNNER_TEMP/host-release-$HOST_VERSION"
host_archive="codex-proxy-rs_${HOST_VERSION}_linux_amd64.tar.gz"
mkdir -p "$download_dir"
gh release download "v${HOST_VERSION}" --repo zyycn/codex-proxy-rs \
  --pattern "$host_archive" --pattern checksums.txt --dir "$download_dir" --clobber
awk -v name="$host_archive" '$2 == name { print; found++ } END { if (found != 1) exit 1 }' \
  "$download_dir/checksums.txt" > "$download_dir/host-checksum.txt"
(cd "$download_dir" && sha256sum --check --strict host-checksum.txt)
tar -xzf "$download_dir/$host_archive" -C "$download_dir"

TMPDIR=$(mktemp -d "$RUNNER_TEMP/integration-$HOST_VERSION.XXXXXX")
export TMPDIR
evidence_dir="dist/verification/$HOST_VERSION"
mkdir -p "$evidence_dir"
collect_evidence() {
  local status=$?
  trap - EXIT
  shopt -s nullglob
  local evidence
  for evidence in "$TMPDIR"/codex-audit-integration-*/result.json "$TMPDIR"/codex-audit-integration-*/*.png; do
    if ! cp -- "$evidence" "$evidence_dir/"; then
      echo "Unable to copy integration evidence: $evidence" >&2
      status=1
    fi
  done
  if [[ "$status" == 0 && ! -s "$evidence_dir/result.json" ]]; then
    echo "Successful integration did not produce result.json for Host $HOST_VERSION" >&2
    status=1
  fi
  exit "$status"
}
trap collect_evidence EXIT

node scripts/integration.js \
  --host-binary "$download_dir/codex-proxy-rs/codex-proxy-rs" \
  --host-web "$download_dir/codex-proxy-rs/web/dist" \
  --package "$PLUGIN_ARCHIVE" \
  --host-version "$HOST_VERSION" \
  --host-commit "$HOST_REVISION" \
  --expect-open-external "$EXPECT_OPEN_EXTERNAL" \
  --next-wait-ms 125000
