#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: image-fingerprint.sh --image NAME --dockerfile PATH --context PATH --paths "PATH [PATH ...]"

Computes a stable image-input fingerprint from tracked files, Docker build
metadata, and resolved base image digests. Writes fingerprint, proof_tag, and
proof_ref to $GITHUB_OUTPUT when present.
EOF
}

image=""
dockerfile=""
context=""
paths=""
proof_repository="${PROOF_REPOSITORY:-shows-build-proof}"
registry_server="${REGISTRY_SERVER:-romainecr.azurecr.io}"
include_base_digests="${INCLUDE_BASE_DIGESTS:-true}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --image)
      image="${2:-}"
      shift 2
      ;;
    --dockerfile)
      dockerfile="${2:-}"
      shift 2
      ;;
    --context)
      context="${2:-}"
      shift 2
      ;;
    --paths)
      paths="${2:-}"
      shift 2
      ;;
    --proof-repository)
      proof_repository="${2:-}"
      shift 2
      ;;
    --registry-server)
      registry_server="${2:-}"
      shift 2
      ;;
    --no-base-digests)
      include_base_digests="false"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "${image}" || -z "${dockerfile}" || -z "${context}" || -z "${paths}" ]]; then
  usage
  exit 2
fi

if [[ ! -f "${dockerfile}" ]]; then
  echo "Dockerfile '${dockerfile}' does not exist" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
manifest="${tmp}/manifest.txt"
: >"${manifest}"

for path in ${paths}; do
  if [[ "${path}" == "." ]]; then
    git ls-files -z \
      | sort -z \
      | xargs -0 sha256sum >>"${manifest}"
  elif [[ -d "${path}" ]]; then
    git ls-files -z -- "${path}" \
      | sort -z \
      | xargs -0 sha256sum >>"${manifest}"
  elif [[ -f "${path}" ]]; then
    sha256sum "${path}" >>"${manifest}"
  else
    echo "Fingerprint input '${path}' does not exist" >&2
    exit 1
  fi
done

{
  printf 'image=%s\n' "${image}"
  printf 'dockerfile=%s\n' "${dockerfile}"
  printf 'context=%s\n' "${context}"
  printf 'buildx=docker/build-push-action@v6\n'
  sha256sum scripts/image-fingerprint.sh
} >>"${manifest}"

if [[ "${include_base_digests}" == "true" ]]; then
  while IFS= read -r base_image; do
    digest="$(docker buildx imagetools inspect "${base_image}" --format '{{json .Manifest.Digest}}' | tr -d '"')"
    printf 'base=%s@%s\n' "${base_image}" "${digest}" >>"${manifest}"
  done < <(
    awk '
      BEGIN { IGNORECASE = 1 }
      $1 == "FROM" {
        image = $2
        if (tolower(image) != "scratch") print image
      }
    ' "${dockerfile}" | sort -u
  )
fi

fingerprint="$(sha256sum "${manifest}" | cut -d' ' -f1)"
proof_tag="${image}-${fingerprint}"
proof_ref="${registry_server}/${proof_repository}:${proof_tag}"

echo "fingerprint=${fingerprint}"
echo "proof_tag=${proof_tag}"
echo "proof_ref=${proof_ref}"

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "fingerprint=${fingerprint}"
    echo "proof_tag=${proof_tag}"
    echo "proof_ref=${proof_ref}"
  } >>"${GITHUB_OUTPUT}"
fi
