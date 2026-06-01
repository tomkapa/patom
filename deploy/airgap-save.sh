#!/usr/bin/env bash
# Pull + save the pinned Patom images into a single tarball for air-gap transfer.
# Usage: ./deploy/airgap-save.sh <tag> [output.tar]
# See doc/operations/self-hosting.md §3 for the full load/push/install steps.
set -euo pipefail

TAG="${1:?usage: airgap-save.sh <tag> [output.tar]}"
OUT="${2:-patom-images-${TAG}.tar}"

IMAGES=(
  "ghcr.io/tomkapa/patom:${TAG}"
  "ghcr.io/tomkapa/patom-backup:${TAG}"
  "pgvector/pgvector:pg17"
)

for img in "${IMAGES[@]}"; do
  echo ">> pulling ${img}"
  docker pull "${img}"
done

echo ">> saving ${#IMAGES[@]} images -> ${OUT}"
docker save "${IMAGES[@]}" -o "${OUT}"
echo "done: ${OUT}"
