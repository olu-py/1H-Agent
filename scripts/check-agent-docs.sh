#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root_doc="$repo_root/AGENTS.md"
legacy_name="AGENT"".""md"
legacy_doc="$repo_root/$legacy_name"

fail() {
    echo "agent docs check failed: $*" >&2
    exit 1
}

test -f "$root_doc" || fail "AGENTS.md is missing"
test ! -e "$legacy_doc" || fail "legacy singular agent document must not exist"
grep -Fq '[AGENTS.md](AGENTS.md)' "$repo_root/README.md" || fail "README.md does not link AGENTS.md"

guides=(provider cluster tui release)
for guide in "${guides[@]}"; do
    relative=".agents/guides/${guide}.md"
    test -f "$repo_root/$relative" || fail "$relative is missing"
    grep -Fq "($relative)" "$root_doc" || fail "AGENTS.md does not route to $relative"
done

if grep -R -n -F "$legacy_name" \
    "$repo_root/README.md" \
    "$root_doc" \
    "$repo_root/.agents" \
    "$repo_root/scripts" \
    "$repo_root/.github"; then
    fail "legacy singular agent document reference found"
fi

echo "agent docs check passed"
