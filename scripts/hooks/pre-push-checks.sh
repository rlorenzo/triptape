#!/usr/bin/env bash
# Pre-push checks mirroring CI. Optional tools are skipped when not installed.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Tauri embeds ../dist, so the app crate needs a built frontend to compile.
if [ ! -d dist ] || [ -n "$(find src index.html package-lock.json vite.config.ts svelte.config.js -newer dist -type f -print -quit 2>/dev/null)" ]; then
  echo "==> npm run build (dist/ missing or stale)"
  [ -d node_modules ] || npm ci
  npm run build
fi

echo "==> cargo clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test"
cargo test --workspace

# CI pins deadwood 1.0.0; other versions (e.g. 1.0.0-beta.1) give false positives.
if command -v deadwood >/dev/null 2>&1 && [ "$(deadwood --version | awk '{print $2}')" = "1.0.0" ]; then
  echo "==> deadwood check"
  cargo fetch --locked
  deadwood check .
else
  echo "skip: deadwood 1.0.0 not installed (cargo install --locked deadwood-rs --version 1.0.0)"
fi

if cargo deny --version >/dev/null 2>&1; then
  echo "==> cargo deny check"
  cargo deny check
else
  echo "skip: cargo-deny not installed (brew install cargo-deny)"
fi

if cargo audit --version >/dev/null 2>&1; then
  echo "==> cargo audit"
  cargo audit
else
  echo "skip: cargo-audit not installed (brew install cargo-audit)"
fi
