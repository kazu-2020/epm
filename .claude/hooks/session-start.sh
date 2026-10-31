#!/bin/bash
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}"

# The cloud container fetches main into a fresh repository instead of cloning, so origin/HEAD is missing and commands that diff against it fail.
git remote set-head origin main
