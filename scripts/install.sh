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
cargo build --locked --release \
  -p golutra-agent-cli \
  -p golutra-agent-tui \
  -p golutra-agent-app-server \
  -p golutra-agent-vis \
  -p golutra-agent-supervisor \
  -p golutra-agent-release \
  -p golutra-agent-eval-worker

install -d -m 755 "$prefix/bin"
install -m 755 target/release/golutra-agent "$prefix/bin/golutra-agent"
install -m 755 target/release/golutra-agent-tui "$prefix/bin/golutra-agent-tui"
install -m 755 target/release/golutra-agent-app-server "$prefix/bin/golutra-agent-app-server"
install -m 755 target/release/golutra-agent-vis "$prefix/bin/golutra-agent-vis"
install -m 755 target/release/golutra-agent-supervisor "$prefix/bin/golutra-agent-supervisor"
install -m 755 target/release/golutra-agent-launcher "$prefix/bin/golutra-agent-launcher"
install -m 755 target/release/golutra-agent-eval-worker "$prefix/bin/golutra-agent-eval-worker"

printf 'Golutra Agent installed in %s/bin\n' "$prefix"
