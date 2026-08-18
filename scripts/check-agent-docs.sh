#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root_doc="$repo_root/AGENTS.md"
legacy_name="AGENT"".""md"
legacy_doc="$repo_root/$legacy_name"
root_line_limit=85
guide_line_limit=50

fail() {
    echo "agent docs check failed: $*" >&2
    exit 1
}

line_count() {
    wc -l < "$1" | tr -d ' '
}

test -f "$root_doc" || fail "AGENTS.md is missing"
test ! -e "$legacy_doc" || fail "legacy singular agent document must not exist"
grep -Fq '[AGENTS.md](AGENTS.md)' "$repo_root/README.md" || fail "README.md does not link AGENTS.md"
test "$(line_count "$root_doc")" -le "$root_line_limit" || fail "AGENTS.md exceeds $root_line_limit lines"

guides=(provider cluster tui release)
sections=("## 适用范围" "## 入口" "## 不变量" "## 诊断" "## 验证")
for guide in "${guides[@]}"; do
    relative=".agents/guides/${guide}.md"
    path="$repo_root/$relative"
    test -f "$path" || fail "$relative is missing"
    test "$(line_count "$path")" -le "$guide_line_limit" || fail "$relative exceeds $guide_line_limit lines"
    grep -Fq "($relative)" "$root_doc" || fail "AGENTS.md does not route to $relative"
    for section in "${sections[@]}"; do
        grep -Fxq "$section" "$path" || fail "$relative is missing section: $section"
    done
done

guide_link_count="$(grep -oE '\.agents/guides/[^)[:space:]]+\.md' "$root_doc" | wc -l | tr -d ' ')"
test "$guide_link_count" -eq "${#guides[@]}" || fail "AGENTS.md must route exactly once to each configured guide"

if grep -R -n -F "$legacy_name" \
    "$repo_root/README.md" "$root_doc" "$repo_root/.agents" \
    "$repo_root/scripts" "$repo_root/.github"; then
    fail "legacy singular agent document reference found"
fi

echo "agent docs check passed"
