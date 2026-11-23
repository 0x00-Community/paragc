#!/usr/bin/env sh
# Builds both books into book/. Uses mdbook from PATH, or downloads the pinned release (Cloudflare's build image has none).
set -eu
cd "$(dirname "$0")"
MDBOOK_VERSION=0.5.4
if ! command -v mdbook >/dev/null 2>&1; then
  mkdir -p .bin
  curl -fsSL "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" | tar -xz -C .bin
  PATH="$PWD/.bin:$PATH"
fi
mdbook build en
mdbook build zh-CN
