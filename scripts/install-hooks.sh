#!/usr/bin/env bash
# Install the pre-commit and pre-push git hooks (see .pre-commit-config.yaml).
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v pre-commit >/dev/null 2>&1; then
  echo "pre-commit not found. Install it with: brew install pre-commit" >&2
  exit 1
fi

# Hook types come from default_install_hook_types in .pre-commit-config.yaml.
pre-commit install
