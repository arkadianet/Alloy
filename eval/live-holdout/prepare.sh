#!/usr/bin/env bash
# Build the live-holdout binary bundle once, from one clean commit.
#
# Usage:
#   ./eval/live-holdout/prepare.sh /path/to/bundle
#
# Produces:
#   <bundle>/target/debug/{alloy,alloy-eval-live-holdout,
#                          alloy-eval-live-naive,alloy-eval-live-repair}
#   <bundle>/manifest.tsv
#
# manifest.tsv is the bundle's identity: the source revision, the clean
# worktree marker, and one sha256 per binary in LC_ALL=C name order. Its own
# sha256 is the binary_bundle_sha256 that every arm of a matrix must share,
# so a rebuild into a shared Cargo target directory can never be mistaken for
# the binaries a matrix actually ran.
#
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd -P)"
bundle="${1:?usage: prepare.sh <bundle-dir>}"

die() { echo "prepare.sh: $1" >&2; exit 2; }

mapfile -t binaries < <(
  printf '%s\n' alloy alloy-eval-live-holdout alloy-eval-live-naive \
    alloy-eval-live-repair | LC_ALL=C sort
)

content_sha() { sha256sum <"$1" | cut -d ' ' -f1; }

# A bundle inside the repository would dirty the worktree with its own build
# output — the very state this script refuses to build from. Reject the path
# before anything is created or compiled.
if [ -d "$bundle" ]; then
  bundle_path="$(cd "$bundle" && pwd -P)"
else
  parent="$(dirname "$bundle")"
  [ -d "$parent" ] || die "bundle parent directory does not exist: $parent"
  bundle_path="$(cd "$parent" && pwd -P)/$(basename "$bundle")"
fi
case "$bundle_path" in
  "$repo" | "$repo"/*)
    die "bundle must live outside the repository $repo, got $bundle_path"
    ;;
esac

# Refuse before creating anything: an existing bundle may still be the
# provenance of a published matrix.
if [ -e "$bundle" ]; then
  [ -d "$bundle" ] || die "bundle path is not a directory: $bundle"
  [ -z "$(ls -A "$bundle")" ] ||
    die "bundle directory must be empty, got existing entries in $bundle"
fi

git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1 ||
  die "$repo is not a Git worktree"
revision="$(git -C "$repo" rev-parse HEAD 2>/dev/null)" ||
  die "repository has no commit to build from"
[[ "$revision" =~ ^[0-9a-f]{40}$ ]] ||
  die "HEAD must resolve to a 40-hex commit sha, got '$revision'"
[ -z "$(git -C "$repo" status --porcelain)" ] ||
  die "worktree must be clean before a bundle is built; commit or stash first"

mkdir -p "$bundle" || die "cannot create bundle directory: $bundle"
CARGO_TARGET_DIR="$bundle/target" cargo build --locked \
  --manifest-path "$repo/Cargo.toml" \
  -p alloy-cli --bin alloy \
  -p alloy-eval --features live-naive \
  --bin alloy-eval-live-repair \
  --bin alloy-eval-live-holdout \
  --bin alloy-eval-live-naive ||
  die "cargo build failed; the bundle is incomplete"

debug="$bundle/target/debug"
for name in "${binaries[@]}"; do
  [ -x "$debug/$name" ] || die "build did not produce an executable $debug/$name"
done

# The build must not have moved the source out from under the manifest.
[ "$(git -C "$repo" rev-parse HEAD)" = "$revision" ] ||
  die "HEAD moved during the build; rebuild the bundle"
[ -z "$(git -C "$repo" status --porcelain)" ] ||
  die "worktree became dirty during the build; rebuild the bundle"

# Written last and atomically: a manifest exists only for a complete bundle.
manifest="$bundle/manifest.tsv"
{
  printf 'source_revision\t%s\n' "$revision"
  printf 'worktree\tclean\n'
  for name in "${binaries[@]}"; do
    printf 'binary\t%s\t%s\n' "$name" "$(content_sha "$debug/$name")"
  done
} >"$manifest.partial" || die "could not write $manifest.partial"
mv "$manifest.partial" "$manifest" || die "could not finalize $manifest"

echo "BUNDLE $bundle"
echo "SOURCE_REVISION $revision"
echo "BINARY_BUNDLE_SHA256 $(content_sha "$manifest")"
