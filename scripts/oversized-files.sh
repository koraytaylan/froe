#!/usr/bin/env bash
# Every Rust source file longer than the given limit (default 1000 lines),
# longest first. Reports the split the file already suggests: how much of it is
# production code and how much is its own `mod tests`.
#
#   scripts/oversized-files.sh          # the 1000-line rule
#   scripts/oversized-files.sh 500      # a stricter sweep
set -euo pipefail
limit=${1:-1000}

printf '%6s %6s %6s  %s\n' total prod tests path
find crates -name '*.rs' -type f -not -path '*/target/*' -print0 |
    while IFS= read -r -d '' path; do
        total=$(wc -l <"$path")
        (( total > limit )) || continue
        tests_at=$(grep -n '^mod tests {' "$path" | head -1 | cut -d: -f1 || true)
        if [ -n "$tests_at" ]; then
            prod=$(( tests_at - 2 ))
        else
            prod=$total
        fi
        printf '%6d %6d %6d  %s\n' "$total" "$prod" "$(( total - prod ))" "$path"
    done | sort -k1 -rn
