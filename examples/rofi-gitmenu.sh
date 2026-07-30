#!/usr/bin/env bash
# Rofi repo picker backed by gitwatch.
# Select a repo -> run `gitwatch sync` on it -> refresh the Waybar module.
set -euo pipefail

DEV_DIR="${GITWATCH_DIR:-$HOME/dev}"
theme_dir="${HOME}/.config/rofi/themes"
theme="gitmenu"

rofi_cmd() {
    rofi -dmenu -sync -i -p "Repos" -theme "${theme_dir}/${theme}.rasi"
}

# `rofi_list` prints "<icon> <name>", dirty repos first. Local check, no fetch
# (fast). Add --fetch if you want ahead/behind reflected in the icons.
list=$(gitwatch rofi_list "${DEV_DIR}")
[ -z "${list}" ] && exit 1

chosen=$(printf '%s\n' "${list}" | rofi_cmd)
[ -z "${chosen}" ] && exit 0

# Strip the leading "<icon> " to recover the repo name.
name="${chosen#* }"
repo="${DEV_DIR}/${name}"

gitwatch sync "${repo}"
rc=$?

# Refresh the Waybar custom module (see waybar-config.jsonc: signal 8).
pkill -RTMIN+8 waybar || true

exit "${rc}"
