#!/bin/bash

for dir in core_lib app/main/src-tauri; do
  find "$dir" -name '*.rs' -not -path "*/target/*" -exec rustfmt {} +
done

# Lint the frontend if it changed
if git diff --name-only HEAD | grep '^app/main/src/' >/dev/null; then
    echo "Changes detected in /app/main/src. Executing script..."
    cd app/main
    pnpm lint --fix
    cd ../..
else
    echo "No changes detected in /app/main/src."
fi
