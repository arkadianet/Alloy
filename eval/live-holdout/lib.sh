#!/usr/bin/env bash
# Shared manifest and tab-record helpers for the live holdout scripts.
#
# Author: arkadianet

mapfile -t binaries < <(
  printf '%s\n' alloy alloy-eval-live-holdout alloy-eval-live-naive \
    alloy-eval-live-repair | LC_ALL=C sort
)

content_sha() { sha256sum <"$1" | cut -d ' ' -f1; }

# Split tabs without collapsing empty columns.
split_row() {
  local rest="$1"
  fields=()
  while [ "$rest" != "${rest#*$'\t'}" ]; do
    fields+=("${rest%%$'\t'*}")
    rest="${rest#*$'\t'}"
  done
  fields+=("$rest")
}
