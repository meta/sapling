#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

set -u
set -o pipefail

usage() {
  cat >&2 <<'EOF'
usage: strider-parity-benchmark.sh \
  --implementation walker|strider \
  --output-prefix PATH \
  --concurrency VALUE \
  --visited-capacity VALUE \
  --cache-condition VALUE \
  --semantic-result match|mismatch|not-checked \
  -- COMMAND [ARG ...]
EOF
}

fail() {
  echo "error: $*" >&2
  usage
  exit 2
}

implementation=
output_prefix=
concurrency=
visited_capacity=
cache_condition=
semantic_result=

while [[ $# -gt 0 ]]; do
  case "$1" in
    --implementation)
      [[ $# -ge 2 ]] || fail "--implementation requires a value"
      implementation=$2
      shift 2
      ;;
    --output-prefix)
      [[ $# -ge 2 ]] || fail "--output-prefix requires a value"
      output_prefix=$2
      shift 2
      ;;
    --concurrency)
      [[ $# -ge 2 ]] || fail "--concurrency requires a value"
      concurrency=$2
      shift 2
      ;;
    --visited-capacity)
      [[ $# -ge 2 ]] || fail "--visited-capacity requires a value"
      visited_capacity=$2
      shift 2
      ;;
    --cache-condition)
      [[ $# -ge 2 ]] || fail "--cache-condition requires a value"
      cache_condition=$2
      shift 2
      ;;
    --semantic-result)
      [[ $# -ge 2 ]] || fail "--semantic-result requires a value"
      semantic_result=$2
      shift 2
      ;;
    --)
      shift
      break
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[[ "$implementation" == "walker" || "$implementation" == "strider" ]] ||
  fail "--implementation must be walker or strider"
[[ -n "$output_prefix" ]] || fail "--output-prefix is required"
[[ "$concurrency" =~ ^[1-9][0-9]*$ ]] || fail "--concurrency must be a positive integer"
[[ -n "$visited_capacity" ]] || fail "--visited-capacity is required"
[[ -n "$cache_condition" ]] || fail "--cache-condition is required"
case "$semantic_result" in
  match|mismatch|not-checked) ;;
  *) fail "--semantic-result must be match, mismatch, or not-checked" ;;
esac
[[ $# -gt 0 ]] || fail "a command is required after --"

for value in "$visited_capacity" "$cache_condition"; do
  [[ "$value" != *$'\n'* ]] || fail "metadata values must fit on one line"
done

sample_file="${output_prefix}.sample"
command_file="${output_prefix}.command"
log_file="${output_prefix}.log"
time_file="${output_prefix}.time"
for path in "$sample_file" "$command_file" "$log_file" "$time_file"; do
  [[ ! -e "$path" ]] || fail "refusing to overwrite $path"
done
mkdir -p "$(dirname "$output_prefix")"

{
  printf 'command='
  printf '%q ' "$@"
  printf '\n'
} > "$command_file"

start_ns=$(date +%s%N)
/usr/bin/time --quiet \
  --format='peak_rss_kb=%M\nmeasured_exit_status=%x' \
  --output="$time_file" \
  -- "$@" > "$log_file" 2>&1
command_status=$?
end_ns=$(date +%s%N)
elapsed_ns=$((end_ns - start_ns))
elapsed_seconds=$(awk -v elapsed_ns="$elapsed_ns" 'BEGIN { printf "%.9f", elapsed_ns / 1000000000 }')
peak_rss_kb=$(sed -n 's/^peak_rss_kb=//p' "$time_file")
measured_exit_status=$(sed -n 's/^measured_exit_status=//p' "$time_file")

write_sample() {
  local sample_status=$1
  local visited_nodes=$2
  local nodes_per_second=$3
  cat > "$sample_file" <<EOF
sample_status=$sample_status
implementation=$implementation
elapsed_seconds=$elapsed_seconds
visited_nodes=$visited_nodes
nodes_per_second=$nodes_per_second
peak_rss_kb=$peak_rss_kb
concurrency=$concurrency
visited_capacity=$visited_capacity
cache_condition=$cache_condition
semantic_result=$semantic_result
exit_status=$command_status
command_file=$command_file
output_log=$log_file
EOF
}

# For a command killed by a signal, `time` exits with 128 plus the signal and
# measures an exit status of 0, so a failure is recorded before the
# measurement is checked.
if [[ $command_status -ne 0 ]]; then
  [[ "$peak_rss_kb" =~ ^[0-9]+$ ]] || peak_rss_kb=unavailable
  write_sample failed unavailable unavailable
  exit "$command_status"
fi

if [[ ! "$peak_rss_kb" =~ ^[0-9]+$ || "$measured_exit_status" != 0 ]]; then
  peak_rss_kb=${peak_rss_kb:-unavailable}
  write_sample invalid unavailable unavailable
  echo "error: resource measurement output is invalid" >&2
  exit 2
fi

case "$implementation" in
  walker)
    # A chunked walk reports one count per chunk.
    mapfile -t visited_counts < <(
      sed -nE 's/.*Seen,Loaded:[[:space:]]*[0-9]+,[[:space:]]*([0-9]+).*/\1/p' "$log_file"
    )
    ;;
  strider)
    mapfile -t visited_counts < <(
      sed -nE 's/.*visited_nodes[^0-9]*([0-9]+).*/\1/p' "$log_file"
    )
    ;;
esac

if [[ ${#visited_counts[@]} -eq 0 ||
  ( "$implementation" == strider && ${#visited_counts[@]} -ne 1 ) ]]; then
  write_sample invalid unavailable unavailable
  echo "error: expected one visited-node count in $log_file, or one per Walker chunk" >&2
  exit 2
fi

visited_nodes=0
for count in "${visited_counts[@]}"; do
  visited_nodes=$(( visited_nodes + count ))
done
nodes_per_second=$(awk -v nodes="$visited_nodes" -v elapsed="$elapsed_seconds" \
  'BEGIN { if (elapsed <= 0) exit 1; printf "%.3f", nodes / elapsed }') || {
  write_sample invalid "$visited_nodes" unavailable
  echo "error: failed to calculate nodes per second" >&2
  exit 2
}

if [[ "$semantic_result" != "match" ]]; then
  write_sample rejected "$visited_nodes" "$nodes_per_second"
  echo "error: semantic result is $semantic_result; refusing a successful benchmark sample" >&2
  exit 2
fi

write_sample success "$visited_nodes" "$nodes_per_second"
