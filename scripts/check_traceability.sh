#!/usr/bin/env bash
# Check the e2e traceability table in ../REFERENCE.md against the tree: every suite row names a
# module tests/e2e/main.rs declares, every declared suite has a row, and each Functions cell lists
# exactly the #[tokio::test] functions its file defines.
# Usage: scripts/check_traceability.sh [path/to/REFERENCE.md]   (run from the repository root)
#
# Exits non-zero on the first mismatch, naming it. REFERENCE.md is outside every repository, so
# this is run by hand whenever the table is edited.
set -euo pipefail
# REFERENCE.md sits beside the main checkout, which a worktree reaches through the common git dir.
REFERENCE=${1:-$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")/../REFERENCE.md}
MAIN=tests/e2e/main.rs

table=$(sed -n '/^### Every e2e suite/,/^Stories with no suite/p' "$REFERENCE" | grep '^| `')
declared=$(grep '^mod ' "$MAIN" | sed 's/^mod \(.*\);/\1/' | grep -vx common | sort)
listed=$(printf '%s\n' "$table" | sed 's/^| `\([a-z_0-9]*\)`.*/\1/' | sort)

if [ "$declared" != "$listed" ]; then
    echo "suite rows and $MAIN disagree (< main.rs only, > table only):"
    diff <(printf '%s\n' "$declared") <(printf '%s\n' "$listed") | grep '^[<>]'
    exit 1
fi

count=$(printf '%s\n' "$declared" | wc -l)
if ! grep -q "declares $count suite modules" "$REFERENCE"; then
    echo "the count sentence does not say $count suite modules"
    exit 1
fi

while IFS= read -r suite; do
    file=tests/e2e/$suite.rs
    [ -f "$file" ] || file=tests/e2e/$suite/mod.rs
    defined=$(grep -A3 '#\[tokio::test\]' "$file" | grep -oE 'async fn [a-z_0-9]+' | sed 's/async fn //' | sort)
    cell=$(printf '%s\n' "$table" | grep "^| \`$suite\` |" | awk -F'|' '{print $5}')
    named=$(printf '%s\n' "$cell" | grep -oE '`[a-z_0-9]+`' | tr -d '`' | sort)
    if [ "$defined" != "$named" ]; then
        echo "$suite: Functions cell and $file disagree (< file only, > table only):"
        diff <(printf '%s\n' "$defined") <(printf '%s\n' "$named") | grep '^[<>]'
        exit 1
    fi
done <<< "$declared"

echo "traceability table matches $MAIN: $count suites"
