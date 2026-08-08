#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

# Construct the tokens so this guard never contains the exact text it scans
# for. Anchoring at column zero avoids prose that merely discusses conflicts.
left=$(printf '<%.0s' {1..7})
middle=$(printf '=%.0s' {1..7})
right=$(printf '>%.0s' {1..7})
fixture='scripts/fixtures/unresolved-conflict.md'

scan() {
    git grep -nI -E \
        -e "^${left}($| )" \
        -e "^${middle}$" \
        -e "^${right}($| )" \
        -- "$@"
}

set +e
matches=$(scan . ":(exclude)${fixture}")
status=$?
set -e

if [[ $status -eq 0 ]]; then
    printf '%s\n' 'Unresolved merge-conflict markers found in tracked text:'
    printf '%s\n' "$matches"
    exit 1
fi
if [[ $status -gt 1 ]]; then
    printf '%s\n' 'Conflict-marker scan failed unexpectedly.' >&2
    exit "$status"
fi

if [[ ${1:-} == '--self-test' ]]; then
    expanded_fixture=$(sed \
        -e "s/^{{LEFT}}/${left}/" \
        -e "s/^{{MIDDLE}}/${middle}/" \
        -e "s/^{{RIGHT}}/${right}/" \
        "$fixture")
    if ! grep -nI -E \
        -e "^${left}($| )" \
        -e "^${middle}$" \
        -e "^${right}($| )" \
        <<<"$expanded_fixture" >/dev/null; then
        printf '%s\n' 'Conflict-marker guard self-test failed to detect its negative fixture.' >&2
        exit 1
    fi
fi

printf '%s\n' 'No unresolved merge-conflict markers found.'
