#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Writes `testtool_drawdag` input for a deterministic history that exercises
# the deleted manifest: mostly commits that only add a file, regular small
# deletions under nested directories, two-branch merges whose branches delete
# different files, and large directories that one commit deletes and a later
# commit partly restores.
#
# Commits are named `C<number>` in creation order, so SQL IDs follow the
# names. Merge commits and their branches carry the suffixes `_left`, `_right`
# and `_merge`; large-directory commits carry `_bigadd`, `_bigdel` and
# `_bigrestore`.

set -euo pipefail

# `MAP_SHARD_SIZE`: a deleted-manifest directory with more entries than this
# keeps them in separately stored sharded-map nodes.
SHARD_SIZE=2000
# Entries a restore leaves deleted: few enough for the map to be inline again.
RESTORED_REMAINDER=1900
# Each large directory holds these subdirectories besides its files, so that
# its shards hold distinct directory nodes, not only the one leaf that every
# file deleted by a commit shares.
LARGE_SUBDIRS=3

usage() {
  cat >&2 <<'EOF'
usage: strider-benchmark-history.sh \
  --commits N \
  --merge-interval N \
  --large-dir-size N \
  --depth N \
  [--large-dirs N]
EOF
}

fail() {
  echo "error: $*" >&2
  usage
  exit 2
}

commits=
merge_interval=
large_dir_size=
depth=
large_dirs=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --commits|--merge-interval|--large-dir-size|--depth|--large-dirs)
      [[ $# -ge 2 ]] || fail "$1 requires a value"
      [[ "$2" =~ ^[1-9][0-9]*$ ]] || fail "$1 must be a positive integer"
      case "$1" in
        --commits) commits=$2 ;;
        --merge-interval) merge_interval=$2 ;;
        --large-dir-size) large_dir_size=$2 ;;
        --depth) depth=$2 ;;
        --large-dirs) large_dirs=$2 ;;
      esac
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[[ -n "$commits" ]] || fail "--commits is required"
[[ -n "$merge_interval" ]] || fail "--merge-interval is required"
[[ -n "$large_dir_size" ]] || fail "--large-dir-size is required"
[[ -n "$depth" ]] || fail "--depth is required"
(( commits <= 99999 )) || fail "--commits must be at most 99999"
(( commits > 4 * large_dirs )) ||
  fail "--commits must exceed four times --large-dirs"
(( merge_interval >= 3 )) || fail "--merge-interval must be at least 3"
(( large_dir_size > SHARD_SIZE )) ||
  fail "--large-dir-size must exceed the shard size, $SHARD_SIZE"

# Each large directory is added, deleted and restored within its own segment
# of the history.
declare -A events
segment=$(( (commits - 1) / large_dirs ))
for (( j = 0; j < large_dirs; j++ )); do
  base=$(( 1 + j * segment ))
  events[$(( base + segment / 4 ))]="bigadd $j"
  events[$(( base + segment / 2 ))]="bigdel $j"
  events[$(( base + 3 * segment / 4 ))]="bigrestore $j"
done

large_files=$(( large_dir_size - LARGE_SUBDIRS ))
restored=$(( large_dir_size - RESTORED_REMAINDER ))

# Small files on the main line, oldest first; `oldest` indexes the oldest one
# that still exists.
live=()
oldest=0
parent=

large_dir() {
  local action=$1 dir=$2 files=$3 k
  for (( k = 0; k < files; k++ )); do
    if [[ "$action" == modify ]]; then
      printf '# modify: %s "big%d/e%05d" "big %d %d"\n' "$name" "$dir" "$k" "$dir" "$k"
    else
      printf '# delete: %s "big%d/e%05d"\n' "$name" "$dir" "$k"
    fi
  done
}

large_subdirs() {
  local action=$1 dir=$2 m
  for (( m = 1; m <= LARGE_SUBDIRS; m++ )); do
    if [[ "$action" == modify ]]; then
      printf '# modify: %s "big%d/sub%d/g%d" "big %d sub %d"\n' "$name" "$dir" "$m" "$m" "$dir" "$m"
    else
      printf '# delete: %s "big%d/sub%d/g%d"\n' "$name" "$dir" "$m" "$m"
    fi
  done
}

echo "# default_files: false"
i=1
while (( i <= commits )); do
  if [[ -n "${events[$i]:-}" ]]; then
    read -r kind dir <<<"${events[$i]}"
    printf -v name 'C%05d_%s' "$i" "$kind"
    printf '%s-%s\n' "$parent" "$name"
    case "$kind" in
      bigadd)
        large_dir modify "$dir" "$large_files"
        large_subdirs modify "$dir"
        ;;
      bigdel)
        large_dir delete "$dir" "$large_files"
        large_subdirs delete "$dir"
        ;;
      bigrestore)
        large_dir modify "$dir" "$restored"
        ;;
    esac
    parent=$name
    i=$(( i + 1 ))
    continue
  fi

  if (( i > 1 && i % merge_interval == 0 && i + 2 <= commits )) &&
    [[ -z "${events[$(( i + 1 ))]:-}" && -z "${events[$(( i + 2 ))]:-}" ]] &&
    (( ${#live[@]} - oldest >= 6 )); then
    printf -v left 'C%05d_left' "$i"
    printf -v right 'C%05d_right' "$(( i + 1 ))"
    printf -v merge 'C%05d_merge' "$(( i + 2 ))"
    printf '%s-%s\n%s-%s\n%s-%s\n%s-%s\n' \
      "$parent" "$left" "$parent" "$right" "$left" "$merge" "$right" "$merge"
    for k in 0 1; do
      printf '# delete: %s "%s"\n' "$left" "${live[oldest + k]}"
    done
    for k in 2 3; do
      printf '# delete: %s "%s"\n' "$right" "${live[oldest + k]}"
    done
    # A merge must state every path on which its parents differ.
    for k in 0 1 2 3; do
      printf '# delete: %s "%s"\n' "$merge" "${live[oldest + k]}"
    done
    oldest=$(( oldest + 4 ))
    parent=$merge
    i=$(( i + 3 ))
    continue
  fi

  printf -v name 'C%05d' "$i"
  if [[ -n "$parent" ]]; then
    printf '%s-%s\n' "$parent" "$name"
  else
    printf '%s\n' "$name"
  fi
  path=tree
  x=$i
  for (( l = 0; l < depth; l++ )); do
    path+="/d$(( x % 3 ))"
    x=$(( x / 3 ))
  done
  printf -v path '%s/f%05d' "$path" "$i"
  printf '# modify: %s "%s" "small %d"\n' "$name" "$path" "$i"
  live+=("$path")
  if (( i % 3 == 0 && ${#live[@]} - oldest >= 4 )); then
    printf '# delete: %s "%s"\n' "$name" "${live[oldest]}"
    oldest=$(( oldest + 1 ))
  fi
  parent=$name
  i=$(( i + 1 ))
done
printf '# bookmark: %s master_bookmark\n' "$parent"
