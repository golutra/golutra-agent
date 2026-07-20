#!/usr/bin/env sh
set -eu

prefix="${HOME}/.local"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --prefix)
      [ "$#" -ge 2 ] || { printf '%s\n' "--prefix requires a path" >&2; exit 2; }
      prefix="$2"
      shift 2
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
cargo build --release \
  -p golutra-cli \
  -p golutra-tui \
  -p golutra-app-server \
  -p golutra-vis \
  -p golutra-supervisor \
  -p golutra-release \
  -p golutra-eval-worker

install -d -m 755 "$prefix/bin"
install -m 755 target/release/golutra-cli "$prefix/bin/golutra"
install -m 755 target/release/golutra-tui "$prefix/bin/golutra-tui"
install -m 755 target/release/golutra-app-server "$prefix/bin/golutra-app-server"
install -m 755 target/release/golutra-vis "$prefix/bin/golutra-vis"
install -m 755 target/release/golutra-supervisor "$prefix/bin/golutra-supervisor"
install -m 755 target/release/golutra-launcher "$prefix/bin/golutra-launcher"
install -m 755 target/release/golutra-eval-worker "$prefix/bin/golutra-eval-worker"

printf 'Golutra installed in %s/bin\n' "$prefix"
