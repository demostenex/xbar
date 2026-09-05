#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
home_dir=${HOME:?HOME must be set}
bin_path=$home_dir/.local/bin/xbar-ai-usage
data_dir=${XDG_DATA_HOME:-$home_dir/.local/share}
service_dir=$data_dir/dbus-1/services
service_path=$service_dir/org.xbar.AiUsage1.service

cargo build --release --manifest-path "$project_dir/Cargo.toml" -p xbar-ai-usage
install -Dm755 "$project_dir/target/release/xbar-ai-usage" "$bin_path"
mkdir -p "$service_dir"
printf '%s\n' \
    '[D-BUS Service]' \
    'Name=org.xbar.AiUsage1' \
    "Exec=$bin_path" >"$service_path"

printf 'installed collector: %s\n' "$bin_path"
printf 'installed D-Bus service: %s\n' "$service_path"
