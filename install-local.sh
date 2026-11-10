#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
fi

cargo build --release --manifest-path "$project_dir/Cargo.toml"
mkdir -p "$HOME/.local/bin"
cp "$project_dir/target/release/mybatis-lsp" "$HOME/.local/bin/mybatis-lsp"
printf 'Installed mybatis-lsp to %s\n' "$HOME/.local/bin/mybatis-lsp"
