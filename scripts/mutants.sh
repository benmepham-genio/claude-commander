#!/usr/bin/env bash
# Run cargo-mutants for claude-commander inside a disposable container.
#
# The repo source is bind-mounted read-only into the container; cargo-mutants
# does its work in a scratch directory inside the container, and writes results
# to ./mutants.out/ via an explicit bind mount.
#
# Usage:
#   ./scripts/mutants.sh                                  # full run per .cargo/mutants.toml
#   ./scripts/mutants.sh --list                           # dry-run, list candidate mutations
#   ./scripts/mutants.sh --list --diff                    # dry-run with diffs
#   ./scripts/mutants.sh --baseline=run                   # baseline test only
#   ./scripts/mutants.sh --file src/tui/digit_accumulator.rs --list
#
# Any args after the script name are passed through to cargo-mutants verbatim.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE_TAG="claude-commander-mutants:latest"
OUTPUT_DIR="${REPO_ROOT}/mutants.out"

cd "${REPO_ROOT}"

# Build the image if it doesn't exist yet. Re-build by removing the image:
#   docker rmi "${IMAGE_TAG}"
if ! docker image inspect "${IMAGE_TAG}" >/dev/null 2>&1; then
    echo "==> Building ${IMAGE_TAG} (first run only)"
    docker build \
        --build-arg MUTANTS_UID="$(id -u)" \
        --build-arg MUTANTS_GID="$(id -g)" \
        -t "${IMAGE_TAG}" \
        "${REPO_ROOT}/docker/mutants"
fi

# Source is mounted read-write so cargo-mutants can create mutants.out/ and
# rotate mutants.out.old/ in place. In default mode (no --in-place flag,
# which we never use) cargo-mutants mutates a scratch copy elsewhere and
# never writes to the source tree itself — only to mutants.out/. The outer
# container is still the real isolation boundary.
echo "==> Running cargo mutants $* (output: ${OUTPUT_DIR})"
exec docker run \
    --rm \
    --init \
    -v "${REPO_ROOT}:/work" \
    --workdir /work \
    "${IMAGE_TAG}" \
    "$@"
