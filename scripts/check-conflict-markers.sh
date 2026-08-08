#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

negative_fixture='scripts/fixtures/unresolved-conflict.md'
positive_fixture='scripts/fixtures/legitimate-markdown.md'

# Read one text stream and succeed only when it contains a complete ordered
# conflict region: an opening run, a later divider, and a later closing run.
# Runs may exceed Git's usual seven characters. Opening/closing labels must be
# absent or separated from the marker by whitespace.
scan_stream() {
    local display_path=$1
    awk -v display_path="$display_path" '
        function marker_run(line, marker, length_seen) {
            length_seen = 0
            while (substr(line, length_seen + 1, 1) == marker) {
                length_seen++
            }
            return length_seen
        }

        function is_boundary(line, marker, length_seen, suffix) {
            length_seen = marker_run(line, marker)
            if (length_seen < 7) {
                return 0
            }
            suffix = substr(line, length_seen + 1)
            return suffix == "" || suffix ~ /^[[:space:]]/
        }

        function is_divider(line, length_seen) {
            length_seen = marker_run(line, "=")
            return length_seen >= 7 && length_seen == length(line)
        }

        {
            line = $0
            sub(/\r$/, "", line)
            if (is_boundary(line, "<")) {
                opening_line = NR
                divider_line = 0
                next
            }
            if (opening_line && !divider_line && is_divider(line)) {
                divider_line = NR
                next
            }
            if (opening_line && divider_line && is_boundary(line, ">")) {
                printf "%s:%d: unresolved conflict region (divider %d, closing %d)\n", \
                    display_path, opening_line, divider_line, NR
                found = 1
                opening_line = 0
                divider_line = 0
            }
        }

        END { exit(found ? 0 : 1) }
    '
}

# Enumerate Git-tracked files from the worktree, preserving arbitrary path
# bytes. `grep -Iq` retains the previous binary-file behavior; empty text files
# cannot contain a marker and are skipped too. The negative fixture is the one
# explicit exclusion because self-test expands it in memory below.
scan_tracked_text() {
    local path matches status
    local found=1
    while IFS= read -r -d '' path; do
        if [[ $path == "$negative_fixture" || ! -f $path || -L $path ]]; then
            continue
        fi
        if ! LC_ALL=C grep -Iq . -- "$path"; then
            continue
        fi
        if matches=$(scan_stream "$path" <"$path"); then
            printf '%s\n' "$matches"
            found=0
        else
            status=$?
            if [[ $status -gt 1 ]]; then
                return "$status"
            fi
        fi
    done < <(git ls-files -z)
    return "$found"
}

if matches=$(scan_tracked_text); then
    printf '%s\n' 'Unresolved merge-conflict regions found in tracked text:'
    printf '%s\n' "$matches"
    exit 1
else
    status=$?
    if [[ $status -gt 1 ]]; then
        printf '%s\n' 'Conflict-marker scan failed unexpectedly.' >&2
        exit "$status"
    fi
fi

if [[ ${1:-} == '--self-test' ]]; then
    left=$(printf '<%.0s' {1..7})
    middle=$(printf '=%.0s' {1..7})
    right=$(printf '>%.0s' {1..7})
    expanded_fixture=$(sed \
        -e "s/^{{LEFT}}/${left}/" \
        -e "s/^{{MIDDLE}}/${middle}/" \
        -e "s/^{{RIGHT}}/${right}/" \
        -e "s/^{{LEFT_LONG}}/${left}<</" \
        -e "s/^{{MIDDLE_LONG}}/${middle}==/" \
        -e "s/^{{RIGHT_LONG}}/${right}>>/" \
        "$negative_fixture")

    if negative_matches=$(scan_stream "$negative_fixture (expanded)" <<<"$expanded_fixture"); then
        region_count=$(printf '%s\n' "$negative_matches" | wc -l | tr -d '[:space:]')
        if [[ $region_count -ne 2 ]]; then
            printf '%s\n' "Conflict-marker guard self-test found ${region_count} regions, expected 2." >&2
            exit 1
        fi
    else
        printf '%s\n' 'Conflict-marker guard self-test missed its negative fixture.' >&2
        exit 1
    fi

    expanded_positive_fixture=$(sed "s/^{{SETEXT}}$/${middle}/" "$positive_fixture")
    if scan_stream "$positive_fixture (expanded)" <<<"$expanded_positive_fixture" >/dev/null; then
        printf '%s\n' 'Conflict-marker guard rejected legitimate Markdown.' >&2
        exit 1
    else
        status=$?
        if [[ $status -gt 1 ]]; then
            printf '%s\n' 'Conflict-marker positive-fixture scan failed unexpectedly.' >&2
            exit "$status"
        fi
    fi
fi

printf '%s\n' 'No unresolved merge-conflict regions found.'
