#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
output_dir="${GOLUTRA_TBENCH_BIN_DIR:-/tmp/golutra-terminal-bench/bin}"
build_dir="${GOLUTRA_TBENCH_BUILD_DIR:-/tmp/golutra-terminal-bench/build}"
rust_image="${GOLUTRA_TBENCH_RUST_IMAGE:-rust:1.93-bookworm}"
verify_image="${GOLUTRA_TBENCH_VERIFY_IMAGE:-debian:bullseye-slim}"
rustup_dist_server="${GOLUTRA_TBENCH_RUSTUP_DIST_SERVER:-${RUSTUP_DIST_SERVER:-https://static.rust-lang.org}}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-dir)
      [ "$#" -ge 2 ] || { printf '%s\n' "--output-dir requires a path" >&2; exit 2; }
      output_dir=$2
      shift 2
      ;;
    --build-dir)
      [ "$#" -ge 2 ] || { printf '%s\n' "--build-dir requires a path" >&2; exit 2; }
      build_dir=$2
      shift 2
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

command -v docker >/dev/null 2>&1 || {
  printf '%s\n' "docker is required" >&2
  exit 1
}

mkdir -p "$output_dir" "$build_dir"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd)
build_dir=$(CDPATH= cd -- "$build_dir" && pwd)

proxy="${GOLUTRA_TBENCH_DOCKER_PROXY:-${HTTPS_PROXY:-${HTTP_PROXY:-}}}"
case "$proxy" in
  *://127.0.0.1:*|*://localhost:*)
    proxy=$(printf '%s' "$proxy" | sed -E 's#://(127\.0\.0\.1|localhost):#://host.docker.internal:#')
    ;;
esac
no_proxy="${NO_PROXY:-localhost,127.0.0.1,::1}"

build_binary() {
  architecture=$1
  platform=$2
  target=$3
  cc_environment=$4
  target_dir="$build_dir/$architecture-target"
  mkdir -p "$target_dir"

  printf 'Building %s for %s\n' "$architecture" "$target"
  docker run --rm --platform "$platform" \
    -e HTTP_PROXY="$proxy" \
    -e HTTPS_PROXY="$proxy" \
    -e ALL_PROXY="$proxy" \
    -e NO_PROXY="$no_proxy" \
    -e RUSTUP_DIST_SERVER="$rustup_dist_server" \
    -e TARGET="$target" \
    -e TARGET_CC_ENV="$cc_environment" \
    -e OUTPUT_NAME="golutra-cli-$architecture.candidate" \
    -v "$root:/src:ro" \
    -v "$target_dir:/target" \
    -v "$output_dir:/out" \
    -w /src \
    "$rust_image" \
    bash -euc '
      apt-get -o Acquire::Retries=5 update
      apt-get -o Acquire::Retries=5 install -y --no-install-recommends musl-tools
      rm -rf /var/lib/apt/lists/*
      for attempt in 1 2 3; do
        if rustup target add "$TARGET"; then
          break
        fi
        [ "$attempt" -lt 3 ] || exit 1
        sleep $((attempt * 5))
      done
      export "$TARGET_CC_ENV=musl-gcc"
      CARGO_TARGET_DIR=/target \
        cargo build --locked --release --target "$TARGET" -p golutra-cli
      install -m 0755 "/target/$TARGET/release/golutra-cli" "/out/$OUTPUT_NAME"
    '
}

verify_binary() {
  architecture=$1
  platform=$2
  binary="/opt/golutra/golutra-cli-$architecture.candidate"

  printf 'Verifying %s in Debian bullseye\n' "$architecture"
  docker run --rm --platform "$platform" \
    -v "$output_dir:/opt/golutra:ro" \
    "$verify_image" \
    "$binary" --help >/dev/null
}

build_binary \
  arm64 \
  linux/arm64 \
  aarch64-unknown-linux-musl \
  CC_aarch64_unknown_linux_musl
build_binary \
  amd64 \
  linux/amd64 \
  x86_64-unknown-linux-musl \
  CC_x86_64_unknown_linux_musl

verify_binary arm64 linux/arm64
verify_binary amd64 linux/amd64

mv "$output_dir/golutra-cli-arm64.candidate" "$output_dir/golutra-cli-arm64"
mv "$output_dir/golutra-cli-amd64.candidate" "$output_dir/golutra-cli-amd64"

printf 'Terminal-Bench binaries written to %s\n' "$output_dir"
