#!/usr/bin/env bash
set -euo pipefail

: "${RELEASE_TAG:?}" "${HOST_VERSION:?}" "${GH_REPO:?}" "${RUNNER_TEMP:?}"
assets=$(realpath dist/release)
download=$(mktemp -d "$RUNNER_TEMP/release-download.XXXXXX")
notes="$RUNNER_TEMP/release-notes.md"

cat > "$notes" <<EOF
Linux x86_64 GNU plugin package; requires glibc 2.34 or newer.
Compatible with codex-proxy-rs ~$HOST_VERSION.

Built and tested by [GitHub Actions]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID), including real-host integration.
Verify downloads with \`sha256sum --check SHA256SUMS\`.
EOF

# A retry may finish an interrupted draft, but must not replace a published asset.
if state=$(gh release view "$RELEASE_TAG" --json isDraft --jq '.isDraft' 2>/dev/null); then
  draft="$state"
else
  gh release create "$RELEASE_TAG" --verify-tag --draft --title "$RELEASE_TAG" \
    --generate-notes --notes-file "$notes"
  draft=true
fi

if [ "$draft" = true ]; then
  gh release upload "$RELEASE_TAG" "$assets"/* --clobber
fi
gh release download "$RELEASE_TAG" --dir "$download"
diff --recursive "$assets" "$download"
(cd "$download" && sha256sum --check SHA256SUMS)
if [ "$draft" = true ]; then
  gh release edit "$RELEASE_TAG" --draft=false
fi

# Check the public download route after publication, without a token.
public=$(mktemp -d "$RUNNER_TEMP/release-public.XXXXXX")
for asset in "$assets"/*; do
  name=$(basename "$asset")
  curl --fail --location --retry 5 --retry-all-errors --silent --show-error \
    "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/releases/download/$RELEASE_TAG/$name" \
    --output "$public/$name"
done
diff --recursive "$assets" "$public"
(cd "$public" && sha256sum --check SHA256SUMS)
