#!/usr/bin/env bash
set -euo pipefail

if [[ "$OSTYPE" == "linux-gnu"* ]]; then
  cargo build --release --target x86_64-unknown-linux-musl
  BINARY="./target/x86_64-unknown-linux-musl/release/assistant-server"
elif [[ "$OSTYPE" == "msys"* || "$OSTYPE" == "cygwin"* || "$OSTYPE" == "win32"* ]]; then
  cargo build --release
  BINARY="./target/release/assistant-server.exe"
else
  echo "Unsupported OS"
  exit 1
fi

mkdir -p data/uploads data/plugins data/logs
export RUST_LOG="info,assistant_server=debug"

if [[ "${1:-}" == "migrate" ]]; then
  sqlx database create
  sqlx migrate run
fi

exec "$BINARY"
