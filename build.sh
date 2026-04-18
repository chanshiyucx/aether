#!/usr/bin/env bash

set -euo pipefail

DEFAULT_PROJECT_DIR="/Users/xin/Developer/shiyu/aether"
PROJECT_DIR="${AETHER_PROJECT_DIR:-$DEFAULT_PROJECT_DIR}"

if [[ ! -f "$PROJECT_DIR/Cargo.toml" ]]; then
  echo "error: could not find Cargo.toml in configured project dir: $PROJECT_DIR" >&2
  echo "hint: set AETHER_PROJECT_DIR if the project has moved" >&2
  exit 1
fi

cd "$PROJECT_DIR"
exec cargo run --manifest-path "$PROJECT_DIR/Cargo.toml" --release -- build "$@"
