#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cargo install --path "$project_dir" --locked --force

printf '%s\n' "Installed Warden. Start it with: warden start"
