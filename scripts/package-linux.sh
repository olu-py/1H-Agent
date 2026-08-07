#!/usr/bin/env bash
set -euo pipefail

name="${1:-1h-agent-linux-x86_64}"
output_dir="${2:-../1H-Agent-Release}"
archive_name="${name}.tar.gz"

mkdir -p "$output_dir"
tar -C target/release -czf "$output_dir/$archive_name" 1h-agent
(
    cd "$output_dir"
    sha256sum "$archive_name" > "$archive_name.sha256"
)
