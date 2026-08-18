#!/usr/bin/env bash
# Refuse any Rust source file longer than the limit (default 1000 lines).
#
# clippy has no lint for this: `too_many_lines` counts a function's body, not
# a file's, and there is nothing in its 2400-odd lints that measures a module.
# So the rule lives here and CI runs it.
#
#   scripts/oversized-files.sh          # the 1000-line rule; exits 1 if broken
#   scripts/oversized-files.sh 500      # a stricter sweep, for finding the next ones
set -euo pipefail
limit=${1:-1000}

offenders=0
while IFS= read -r -d '' path; do
    total=$(wc -l <"$path")
    (( total > limit )) || continue
    if (( offenders == 0 )); then
        printf '%6s %6s %6s  %s\n' total prod tests path
    fi
    tests_at=$(grep -n '^mod tests {' "$path" | head -1 | cut -d: -f1 || true)
    prod=${tests_at:+$(( tests_at - 2 ))}
    prod=${prod:-$total}
    printf '%6d %6d %6d  %s\n' "$total" "$prod" "$(( total - prod ))" "$path"
    offenders=$(( offenders + 1 ))
done < <(find crates -name '*.rs' -type f -not -path '*/target/*' -print0 | sort -z)

if (( offenders )); then
    echo >&2
    echo "$offenders file(s) over $limit lines. A file this long is telling you it" >&2
    echo "holds more than one subject; split it at the seams its own sections" >&2
    echo "already have, and move each test to the module it now belongs with." >&2
    exit 1
fi
