#!/bin/bash
# Wrapper around systemd-nspawn for running devtools (mkarchroot / makechrootpkg /
# arch-nspawn) inside a plain container that has no systemd manager or system bus.
#
# devtools' arch-nspawn always passes `--slice=` and lets nspawn allocate a
# transient scope unit, which requires talking to a systemd manager over the
# system bus. Inside a container there is none, so nspawn fails with
# "Failed to open system bus". We strip `--slice=` and force `--keep-unit`
# (reuse the current cgroup instead of creating a scope) plus `--register=no`,
# which needs neither a manager nor a bus.
set -euo pipefail

args=()
have_keep_unit=0
have_register=0
for a in "$@"; do
  case "$a" in
    --slice=*) ;; # drop: no manager to own a slice
    --keep-unit) have_keep_unit=1; args+=("$a") ;;
    --register=*) have_register=1; args+=("$a") ;;
    *) args+=("$a") ;;
  esac
done

prefix=()
[[ $have_keep_unit -eq 0 ]] && prefix+=(--keep-unit)
[[ $have_register -eq 0 ]] && prefix+=(--register=no)

exec /usr/bin/systemd-nspawn "${prefix[@]}" "${args[@]}"
