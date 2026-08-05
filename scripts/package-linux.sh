#!/usr/bin/env bash
set -euo pipefail

name="${1:-1h-agent-linux-x86_64}"
mkdir -p dist
tar -C target/release -czf "dist/${name}.tar.gz" 1h-agent
sha256sum "dist/${name}.tar.gz" > "dist/${name}.tar.gz.sha256"

