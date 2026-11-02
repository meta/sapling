#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Library routines and initial setup for Mononoke-related tests.

# This runs the Python version of functions on debugruntest. It's also
# capable of importing the env vars that would be modified if the function
# was written in Bash.
function python_fn() {
  echo -n "" > "$TESTTMP/.dbrtest_envs"
  # Launchers call this, and a test may launch under a restricted PATH; the
  # test runner exports BINDIR for the hg it built.
  CHGDISABLE=1 "${BINDIR:+$BINDIR/}hg" debugpython "$TEST_FIXTURES/dbrtest_runner.py" "$@"
  local rv=$?
  # shellcheck disable=SC1091
  . "$TESTTMP/.dbrtest_envs"
  return $rv
}

SCS_GFLAGS_ARGS=()
if [ -n "$FB_TEST_FIXTURES" ] && [ -f "$FB_TEST_FIXTURES/fb_library.sh" ]; then
  # shellcheck source=fbcode/eden/mononoke/tests/integration/facebook/fb_library.sh
  . "$FB_TEST_FIXTURES/fb_library.sh"
fi

python_fn setup_environment_variables

# Redirect LLVM profiling output to $TESTTMP so coverage-instrumented binaries
# (e.g. hg) don't write default.profraw into test repo working directories,
# where it would be picked up by "hg commit -Am" and break test expectations.
export LLVM_PROFILE_FILE="$TESTTMP/default_%p.profraw"

# Suppress diagnostic info logs from repo initialization facets that would
# otherwise appear in test output and break .t file expectations.
export RUST_LOG="${RUST_LOG:-info,repo_factory=WARN,dbbookmarks=WARN,warm_bookmarks_cache=WARN}"

function urlencode {
  python_fn urlencode "$@"
}

# Render $TESTTMP/mononoke-config into the manifest + RepoSpec layout under
# $LOCAL_CONFIGERATOR_PATH. Every launcher calls this, then passes
# `--config-tier "$MONONOKE_CONFIG_TIER"`.
function render_mononoke_config {
  python_fn render_mononoke_config
}

function get_configerator_relative_path {
  realpath --relative-to "${LOCAL_CONFIGERATOR_PATH}" "$1"
}

case "$(uname -s)" in
  # Workarounds for running tests on MacOS
  Darwin*)
    prefix="${HOMEBREW_PREFIX:-/usr/local}"

    # Use the brew installed versions of GNU utils
    PATH="$prefix/opt/gnu-sed/libexec/gnubin:\
$prefix/opt/grep/libexec/gnubin:\
$prefix/opt/coreutils/libexec/gnubin:\
$PATH"
  ;;
esac

function killandwait {
  # sends KILL to the given process and waits for it so that nothing is printed
  # to the terminal on MacOS
  { kill -9 "$1" && tail --pid="$1" -f /dev/null; } > /dev/null 2>&1
  # We don't care for wait exit code
  true
}

function termandwait {
  # sends TERM to the given process and waits for it so that nothing is printed
  # to the terminal on MacOS
  { kill -s SIGTERM "$1" && tail --pid="$1" -f /dev/null; } > /dev/null 2>&1
  # We don't care for wait exit code
  true
}

function get_free_socket {
  # Redirect PAR unpacking to /tmp to avoid Permission denied errors on
  # SandCastle workers where /dev/shm may be mounted noexec (T262741687).
  # FB_PAR_UNPACK_BASEDIR controls where the PAR runtime binary is extracted;
  # PAR_TEMP_DIR alone is insufficient for newer PAR formats.
  FB_PAR_UNPACK_BASEDIR="${FB_PAR_UNPACK_BASEDIR:-/tmp}" PAR_TEMP_DIR="${PAR_TEMP_DIR:-/tmp}" "$GET_FREE_SOCKET"
}

ZELOS_PORT=$(get_free_socket)
export ZELOS_PORT

CAS_SERVER_SOCKET=$(get_free_socket)

function mononoke_host {
  python_fn mononoke_host "$@"
}

function mononoke_address {
  python_fn mononoke_address "$@"
}

function cas_server_address {
  if [[ $LOCALIP == *":"* ]]; then
    # ipv6, surround in brackets
    echo -n "[$LOCALIP]:$CAS_SERVER_SOCKET"
  else
    echo -n "$LOCALIP:$CAS_SERVER_SOCKET"
  fi
}


function scs_address {
  echo -n "$(mononoke_host):$SCS_PORT"
}

function land_service_address {
  echo -n "$(mononoke_host):$LAND_SERVICE_PORT"
}

function multi_repo_land_service_address {
  # shellcheck disable=SC2119
  echo -n "$(mononoke_host):$MULTI_REPO_LAND_SERVICE_PORT"
}

function mononoke_git_service_address {
  echo -n "$(mononoke_host):$MONONOKE_GIT_SERVICE_PORT"
}

function dds_address {
  echo -n "$(mononoke_host):$DDS_PORT"
}

# return random value from [1, max_value]
function random_int() {
  max_value=$1

  VAL=$RANDOM
  (( VAL %= max_value ))
  (( VAL += 1 ))

  echo $VAL
}

function sslcurlas {
  python_fn sslcurlas "$@"
}

function sslcurl {
  python_fn sslcurl "$@"
}

function sslcurl_noclientinfo_test {
  curl --noproxy localhost --cert "$TEST_CERTDIR/proxy.crt" --cacert "$TEST_CERTDIR/root-ca.crt" --key "$TEST_CERTDIR/proxy.key" "$@"
}

function curltest {
  curl -H 'x-client-info: {"request_info": {"entry_point": "CurlTest", "correlator": "test"}}' "$@"
}

function mononoke {
  python_fn mononoke "$@"
}

function mononoke_cas_sync {
  HG_REPO_NAME="$1"
  shift
  START_ID="$1"
  shift

  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_CAS_SYNC" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --retry-num 1 \
    --repo-name $HG_REPO_NAME \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
     sync-loop --start-id "$START_ID" --batch-size 20 "$@"
}


function mononoke_walker {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_WALKER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --repo-id $REPOID \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function mononoke_blobstore_healer {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_BLOBSTORE_HEALER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@" 2>&1 | grep -v "Could not connect to a replica"
}

function mononoke_sqlblob_gc {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_SQLBLOB_GC" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@" 2>&1 | grep -v "Could not connect to a replica"
}

function mononoke_x_repo_sync() {
  source_repo_id=$1
  target_repo_id=$2
  shift
  shift
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_X_REPO_SYNC" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --source-repo-id="$source_repo_id" \
    --target-repo-id="$target_repo_id" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --scuba-log-file "$TESTTMP/x_repo_sync_scuba_logs" \
    --tracing-test-format \
    "$@"
}

function create_books_sqlite3_db {
  cat >> "$TESTTMP"/bookmarks.sql <<SQL
  CREATE TABLE IF NOT EXISTS bookmarks_update_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  repo_id INT UNSIGNED NOT NULL,
  name VARCHAR(512) NOT NULL,
  from_changeset_id VARBINARY(32),
  to_changeset_id VARBINARY(32),
  reason VARCHAR(32) NOT NULL, -- enum is used in mysql
  category VARCHAR(32) NOT NULL DEFAULT 'branch',
  timestamp BIGINT NOT NULL
);
SQL

  sqlite3 "$TESTTMP/monsql/sqlite_dbs" < "$TESTTMP"/bookmarks.sql
}

function mononoke_modern_sync {
  FLAGS="$1"
  COMMAND="$2"
  ORIG_REPO="$3"
  DEST_REPO="$4"
  shift
  shift
  shift
  shift

  if [ -n "$FLAGS" ]; then
    FLAGS_ARG="$FLAGS"
  else
    FLAGS_ARG=""
  fi
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_MODERN_SYNC" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --bookmark "master_bookmark" \
    --exit-file "exit_file" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --dest-socket $MONONOKE_SOCKET \
    --tls-ca "$TEST_CERTDIR/root-ca.crt" \
    --tls-private-key "$TEST_CERTDIR/localhost.key" \
    --tls-certificate "$TEST_CERTDIR/localhost.crt" \
    --scuba-log-file "$TESTTMP/modern_sync_scuba_logs" \
    --tracing-test-format \
    ${FLAGS_ARG:+$FLAGS_ARG} \
    "$COMMAND" \
    --repo-name "$ORIG_REPO" \
    --dest-repo-name "$DEST_REPO" \
    "$@"
}

function mononoke_admin {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_ADMIN" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --log-level INFO \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function mononoke_import {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_IMPORT" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function mononoke_testtool {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_TESTTOOL" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function repo_metadata_logger {
  render_mononoke_config
  GLOG_minloglevel=5 "$REPO_METADATA_LOGGER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --scuba-log-file "$TESTTMP/metadata_logger_scuba_logs" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function commit_metadata_scraper {
  render_mononoke_config
  GLOG_minloglevel=5 "$COMMIT_METADATA_SCRAPER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function wait_for_json_record_count {
  # We ask jq to count records for us, so that we're a little more robust ot
  # newlines and such.
  local file count
  file="$1"
  count="$2"

  for _ in $(seq 1 50); do
    if [[ "$(jq 'true' < "$file" | wc -l)" -eq "$count" ]] ; then
      return 0
    fi

    sleep 0.1
  done

  echo "File $file did not contain $count records, it had $(jq 'true' < "$file" | wc -l)" >&2
  jq -S . < "$file" >&2
  return 1
}

function wait_for_server {
  # TODO: Delete the shell version of this function once we can totally replace it with the Python version
  local service_description port_env_var log_file timeout_secs bound_addr_file
  service_description="$1"; shift
  port_env_var="$1"; shift
  log_file="$1"; shift
  timeout_secs="$1"; shift
  bound_addr_file="$1"; shift

  local start
  start=$(date +%s)

  local found_port
  while [[ $(($(date +%s) - start)) -lt "$timeout_secs" ]]; do
    if [[ -z "$found_port" && -r "$bound_addr_file" ]]; then
      found_port=$(sed 's/^.*:\([^:]*\)$/\1/' "$bound_addr_file")
      eval "$port_env_var"="$found_port"
    fi
    if [[ -n "$found_port" ]] && "$@" >/dev/null 2>&1 ; then
      return 0
    fi
    sleep 1
  done

  echo "$service_description did not start in $timeout_secs seconds, took $(($(date +%s) - start))" >&2
  if [[ -n "$found_port" ]]; then
    echo "Running check: $* >/dev/null"
    "$@" >/dev/null
    echo "exited with $?" 1>&2
  else
    echo "Port was never written to $bound_addr_file" 1>&2
  fi
  echo "" 1>&2
  echo "Log of $service_description" 1>&2
  cat "$log_file" 1>&2
  exit 1
}

function mononoke_health {
  python_fn mononoke_health "$@"
}

# Wait until a Mononoke server is available for this repo.
function wait_for_mononoke {
  python_fn wait_for_mononoke "$@"
}

function flush_mononoke_bookmarks {
  sslcurl -X POST -fsS "https://localhost:$MONONOKE_SOCKET/control/drop_bookmarks_cache"
}

function sync_mononoke_warm_bookmarks_cache {
  sslcurl -X POST -fsS "https://localhost:$MONONOKE_SOCKET/control/sync_warm_bookmarks_cache"
}

function force_update_configerator {
  sslcurl -X POST -fsS "https://localhost:$MONONOKE_SOCKET/control/force_update_configerator"
}

# We can't use the "with client certs" option everywhere
# because it breaks connecting to ephemeral mysql instances.
function start_and_wait_for_mononoke_server_with_client_certs {
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/proxy.crt" \
    THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/proxy.key" \
    mononoke "$@"
    wait_for_mononoke
}

function start_and_wait_for_mononoke_server {
    mononoke "$@"
    wait_for_mononoke
}

# Wait until cache warmup finishes
function wait_for_mononoke_cache_warmup {
  local attempts=150
  for _ in $(seq 1 $attempts); do
    grep -q "finished initial warmup" "$TESTTMP/mononoke.out" && break
    sleep 0.1
  done

  if ! grep -q "finished initial warmup" "$TESTTMP/mononoke.out"; then
    echo "Mononoke warmup did not finished" >&2
    cat "$TESTTMP/mononoke.out"
    exit 1
  fi
}

function setup_common_hg_configs {
  python_fn setup_common_hg_configs "$@"
}

function setup_common_config {
    python_fn setup_common_config "$@"
    # TODO: get rid of the cd once python_fn can properly switch cd
    cd "$TESTTMP/mononoke-config" || exit 1
}

function get_bonsai_svnrev_mapping {
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select hex(bcs_id), svnrev from bonsai_svnrev_mapping order by id";
}

function get_bonsai_git_mapping {
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select hex(bcs_id), hex(git_sha1) from bonsai_git_mapping order by repo_id, bcs_id";
}

function get_bonsai_hg_mapping {
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select hex(bcs_id), hex(hg_cs_id) from bonsai_hg_mapping order by repo_id, bcs_id";
}

function get_bonsai_globalrev_mapping {
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select hex(bcs_id), globalrev from bonsai_globalrev_mapping order by globalrev";
}

function set_bonsai_globalrev_mapping {
  REPO_ID="$1"
  BCS_ID="$2"
  GLOBALREV="$3"
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" "INSERT INTO bonsai_globalrev_mapping (repo_id, bcs_id, globalrev) VALUES ($REPO_ID, X'$BCS_ID', $GLOBALREV)";
}

function set_mononoke_as_source_of_truth_for_git {
  REPO_NAME_HEX=$(echo -n "${REPONAME}" | xxd -p)
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" "REPLACE INTO git_repositories_source_of_truth (repo_id, repo_name, source_of_truth) VALUES (${REPO_ID:-0}, X'${REPO_NAME_HEX}', 'mononoke')"
}

function setup_mononoke_config {
  python_fn setup_mononoke_config "$@"
  cd "$TESTTMP/mononoke-config" || exit 1
}

function setup_acls() {
  python_fn setup_acls "$@"
}

function db_config() {
  python_fn db_config "$@"
}

function ephemeral_db_config() {
  python_fn ephemeral_db_config "$@"
}

function blobstore_db_config() {
  python_fn blobstore_db_config
}

function setup_mononoke_storage_config {
  python_fn setup_mononoke_storage_config "$@"
}

function setup_commitsyncmap {
  cp "$TEST_FIXTURES/commitsync/current.toml" "$TESTTMP/mononoke-config/common/commitsyncmap.toml"
}

function setup_configerator_configs {
  python_fn setup_configerator_configs "$@"
}

function setup_mononoke_repo_config {
  # TODO: get rid of the cd once python_fn can properly switch cd
  cd "$TESTTMP/mononoke-config" || exit

  python_fn setup_mononoke_repo_config "$@"
}

function write_infinitepush_config {
  python_fn write_infinitepush_config "$@"
}

function register_hook {
  hook_name="$1"

  shift 1
  EXTRA_CONFIG_DESCRIPTOR=""
  if [[ $# -gt 0 ]]; then
    EXTRA_CONFIG_DESCRIPTOR="$1"
  fi

  reponame_urlencoded="$(urlencode encode "$REPONAME")"
  (
    cat <<CONFIG
[[bookmarks.hooks]]
hook_name="$hook_name"
[[hooks]]
name="$hook_name"
CONFIG
    [ -n "$EXTRA_CONFIG_DESCRIPTOR" ] && cat "$EXTRA_CONFIG_DESCRIPTOR"
  ) >> "repos/$reponame_urlencoded/server.toml"
}

function register_hook_limit_filesize_global_limit {
global_limit=$1
shift 1

register_hook limit_filesize <(
cat <<CONF
config_string_lists={filesize_limits_regexes=[".*"]}
config_int_lists={filesize_limits_values=[$global_limit]}
$@
CONF
)

}

function backfill_mapping {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_BACKFILL_MAPPING" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function bonsai_verify {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_BONSAI_VERIFY" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function s_client {
    /usr/local/fbcode/platform010/bin/openssl s_client \
        -connect "$(mononoke_address)" \
        -CAfile "${TEST_CERTDIR}/root-ca.crt" \
        -cert "${TEST_CERTDIR}/client0.crt" \
        -key "${TEST_CERTDIR}/client0.key" \
        -ign_eof "$@"
}

function scs {
  if [[ ! -d "$SCRIBE_LOGS_DIR" ]]; then
    mkdir "$SCRIBE_LOGS_DIR"
  fi

  # Disable bookmark cache unless test opts in with ENABLE_BOOKMARK_CACHE=1.
  local BOOKMARK_CACHE_FLAG
  if [ -z "$ENABLE_BOOKMARK_CACHE" ]; then
    BOOKMARK_CACHE_FLAG="--disable-bookmark-cache-warming"
  fi

  rm -f "$TESTTMP/scs_server_addr.txt"
  render_mononoke_config
  GLOG_minloglevel=5 \
    THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
    THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/proxy.crt" \
    THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/proxy.key" \
    THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
    "$SCS_SERVER" "$@" \
    --host "$LOCALIP" \
    --port 0 \
    --log-level DEBUG \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --bound-address-file "$TESTTMP/scs_server_addr.txt" \
    --scribe-logging-directory "$TESTTMP/scribe_logs" \
    --tracing-test-format \
    --runtime-thread-stack-size 8388608 \
    "${SCS_GFLAGS_ARGS[@]}" \
    $BOOKMARK_CACHE_FLAG \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" >> "$TESTTMP/scs_server.out" 2>&1 &
  export SCS_SERVER_PID=$!
  echo "$SCS_SERVER_PID" >> "$DAEMON_PIDS"
}

function land_service {
  if [[ ! -d "$SCRIBE_LOGS_DIR" ]]; then
    mkdir "$SCRIBE_LOGS_DIR"
  fi
  rm -f "$TESTTMP/land_service_addr.txt"
  render_mononoke_config
  GLOG_minloglevel=5 \
    THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
    THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
    "$LAND_SERVICE" "$@" \
    --host "$LOCALIP" \
    --port 0 \
    --log-level DEBUG \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --scribe-logging-directory "$TESTTMP/scribe_logs" \
    --bound-address-file "$TESTTMP/land_service_addr.txt" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" >> "$TESTTMP/land_service.out" 2>&1 &
  export LAND_SERVICE_PID=$!
  echo "$LAND_SERVICE_PID" >> "$DAEMON_PIDS"
}

function wait_for_scs {
  export SCS_PORT
  wait_for_server "SCS server" SCS_PORT "$TESTTMP/scs_server.out" \
    "${MONONOKE_SCS_START_TIMEOUT:-"$MONONOKE_SCS_DEFAULT_START_TIMEOUT"}" "$TESTTMP/scs_server_addr.txt" \
    scsc repos
}

# Because of warm bookmark cache the SCS bookmark updates are async.
# This function allows to wait for them.
function wait_for_bookmark_update() {
  local repo="$1"
  local bookmark="$2"
  local target="$3"
  local scheme="${4:-hg}"
  local sleep_time="${SLEEP_TIME:-2}"
  local attempt=1
  sleep "$sleep_time"
  while [[ "$(scsc lookup -S "$scheme" -R "$repo" -B "$bookmark")" != "$target" ]]
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt 5 ]]
    then
        echo "bookmark move of $bookmark to $target has not happened"
        return 1
    fi
    sleep "$sleep_time"
  done
}

function wait_for_bookmark_delete() {
  local repo=$1
  local bookmark=$2
  local attempt=1
  sleep 2
  while scsc lookup -R $repo -B $bookmark 2>/dev/null
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt 5 ]]
    then
       echo "bookmark deletion of $bookmark has not happened"
       return 1
    fi
    sleep 2
  done
}

function get_bookmark_value_edenapi {
  local repo="$1"
  local bookmark="$2"
  hg debugapi mono:"$repo" -e bookmarks -i "[\"$bookmark\"]" | jq -r ".\"$bookmark\""
}

function wait_for_bookmark_move_away_edenapi() {
  local repo="$1"
  local bookmark="$2"
  local prev="$3"
  local max_attempts=${ATTEMPTS:-30}
  local attempt=1
  sleep 1
  flush_mononoke_bookmarks
  while [[ "$(get_bookmark_value_edenapi "$repo" "$bookmark")" == "$prev" ]]
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt $max_attempts ]]
    then
        echo "bookmark move of $bookmark away from $prev has not happened"
        return 1
    fi
    sleep 2
    flush_mononoke_bookmarks
  done
}

function get_bookmark_value_bonsai {
  local repo="$1"
  local bookmark="$2"
  mononoke_admin bookmarks -R "$repo" get "$bookmark"
}

function wait_for_bookmark_move_away_bonsai() {
  local repo="$1"
  local bookmark="$2"
  local prev="$3"
  local max_attempts=${ATTEMPTS:-30}
  local attempt=1
  sleep 1
  flush_mononoke_bookmarks
  while [[ "$(get_bookmark_value_bonsai "$repo" "$bookmark")" == "$prev" ]]
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt $max_attempts ]]
    then
        echo "bookmark move of $bookmark away from $prev has not happened"
        return 1
    fi
    sleep 2
    flush_mononoke_bookmarks
  done
}

function wait_for_bookmark_move_edenapi() {
  local repo="$1"
  local bookmark="$2"
  local target="$3"
  local attempt=1
  sleep 1
  flush_mononoke_bookmarks
  while [[ "$(get_bookmark_value_edenapi "$repo" "$bookmark")" != "$target" ]]
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt 30 ]]
    then
        echo "bookmark move of $bookmark away to $target has not happened"
        return 1
    fi
    sleep 2
    flush_mononoke_bookmarks
  done
}

function wait_for_git_bookmark_move() {
  local bookmark_name="$1"
  local last_bookmark_target="$2"
  local attempt=1
  local max_attempts=${MAX_ATTEMPTS:-30}
  local sleep_time=${SLEEP_TIME:-2}
  last_status_regex="$last_bookmark_target\s+$bookmark_name"
  last_status="$last_bookmark_target$bookmark_name"
  while [[ "$(git_client ls-remote --quiet | grep -E "$last_status_regex" | tr -d '[:space:]')" == "$last_status" ]]
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt $max_attempts ]]
    then
        echo "bookmark move of $bookmark away from $last_bookmark_target has not happened"
        return 1
    fi
    sleep $sleep_time
  done
}

function wait_for_git_bookmark_delete() {
  local bookmark_name="$1"
  local attempt=0
  while [ $attempt -lt 30 ]
  do
    attempt=$((attempt + 1))
    refs=$(git_client ls-remote --quiet)
    if echo "$refs" | grep -q "$bookmark_name"; then
      sleep 2
    else
      return 0
    fi
  done
}

function wait_for_git_bookmark_create() {
  local bookmark_name="$1"
  local attempt=0
  while [ $attempt -lt 30 ]
  do
    attempt=$((attempt + 1))
    refs=$(git_client ls-remote --quiet)
    if echo "$refs" | grep -q "$bookmark_name"; then
      return 0
    else
      sleep 2
    fi
  done
}


function wait_for_land_service {
  export LAND_SERVICE_PORT
  wait_for_server "Land service" LAND_SERVICE_PORT "$TESTTMP/land_service.out" \
    "${MONONOKE_LAND_SERVICE_START_TIMEOUT:-"$MONONOKE_LAND_SERVICE_DEFAULT_START_TIMEOUT"}" "$TESTTMP/land_service_addr.txt" \
    sleep 5
}

function start_and_wait_for_scs_server {
  scs "$@"
  wait_for_scs
}

function start_and_wait_for_land_service {
  land_service "$@"
  wait_for_land_service
}

function add_repo_manifest_mapping_entry {
  # Routes a member (repo, branch) to a manifest branch — the store the
  # manifest tailer maintains in production.
  # Schema copied verbatim from
  # repo_attributes/repo_manifest_mapping/schemas/sqlite-repo-manifest-mapping.sql
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" <<'EOSQL'
CREATE TABLE IF NOT EXISTS `repo_manifest_mapping` (
  `manifest_repo_id` INTEGER NOT NULL,
  `manifest_branch` VARBINARY(255) NOT NULL,
  `repo_name` VARBINARY(255) NOT NULL,
  `repo_branch` VARBINARY(255) NOT NULL,
  PRIMARY KEY (`manifest_repo_id`, `manifest_branch`, `repo_name`, `repo_branch`)
);
CREATE INDEX IF NOT EXISTS `reverse_idx` ON `repo_manifest_mapping` (`repo_name`, `repo_branch`);
CREATE TABLE IF NOT EXISTS `manifest_watermark` (
  `repo_id` INTEGER NOT NULL,
  `manifest_branch` VARBINARY(255) NOT NULL,
  `log_id` BIGINT NOT NULL,
  `last_landed_at` BIGINT NULL,
  PRIMARY KEY (`repo_id`, `manifest_branch`)
);
CREATE INDEX IF NOT EXISTS `read_cursor_idx` ON `manifest_watermark` (`repo_id`, `log_id`);
EOSQL
  local manifest_branch=${2//"'"/"''"}
  local repo_name=${3//"'"/"''"}
  local repo_branch=${4//"'"/"''"}
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" \
    "INSERT OR REPLACE INTO repo_manifest_mapping (manifest_repo_id, manifest_branch, repo_name, repo_branch) VALUES ($1, CAST('$manifest_branch' AS BLOB), CAST('$repo_name' AS BLOB), CAST('$repo_branch' AS BLOB))"
}

function set_manifest_branch_activity {
  # Stamps a manifest branch's last_landed_at (unix seconds), as the tailer or
  # the land service would; the branch must already have a mapping entry.
  local manifest_branch=${2//"'"/"''"}
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" \
    "INSERT INTO manifest_watermark (repo_id, manifest_branch, log_id, last_landed_at) VALUES ($1, CAST('$manifest_branch' AS BLOB), 0, $3) ON CONFLICT(repo_id, manifest_branch) DO UPDATE SET last_landed_at = $3"
}

function multi_repo_land_service {
  # Ensure the git_repositories_source_of_truth tables exist. These are normally
  # created by gitimport but MLR tests may use testtool_drawdag instead.
  # Schema from repo_attributes/git_source_of_truth/schemas/sqlite-git-repositories-source-of-truth.sql
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" <<'EOSQL'
CREATE TABLE IF NOT EXISTS source_of_truth_type (
  source_of_truth VARCHAR(20) PRIMARY KEY NOT NULL,
  sequence INTEGER NOT NULL
);
INSERT OR REPLACE INTO source_of_truth_type (source_of_truth, sequence)
  VALUES ('mononoke', 1), ('metagit', 2), ('locked', 3);
CREATE TABLE IF NOT EXISTS git_repositories_source_of_truth (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  repo_id INTEGER NOT NULL,
  repo_name VARCHAR(255) NOT NULL,
  source_of_truth VARCHAR(20) NOT NULL DEFAULT ('locked')
    REFERENCES source_of_truth_type (source_of_truth),
  mutation_id INTEGER DEFAULT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS repo_id_idx
  ON git_repositories_source_of_truth (repo_id);
CREATE UNIQUE INDEX IF NOT EXISTS repo_name_idx
  ON git_repositories_source_of_truth (repo_name);
CREATE INDEX IF NOT EXISTS source_of_truth_idx
  ON git_repositories_source_of_truth (source_of_truth);
CREATE INDEX IF NOT EXISTS mutation_id_idx
  ON git_repositories_source_of_truth (mutation_id);
EOSQL
  rm -f "$TESTTMP/multi_repo_land_service_addr.txt"
  render_mononoke_config
  GLOG_minloglevel=5 \
    THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
    THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
    "$MULTI_REPO_LAND_SERVICE" "$@" \
    --host "$LOCALIP" \
    --port 0 \
    --log-level DEBUG \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --bound-address-file "$TESTTMP/multi_repo_land_service_addr.txt" \
    --scuba-log-file "$TESTTMP/multi_repo_land_service_scuba.json" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" >> "$TESTTMP/multi_repo_land_service.out" 2>&1 &
  export MULTI_REPO_LAND_SERVICE_PID=$!
  echo "$MULTI_REPO_LAND_SERVICE_PID" >> "$DAEMON_PIDS"
}

MONONOKE_MULTI_REPO_LAND_SERVICE_DEFAULT_START_TIMEOUT=60

function wait_for_multi_repo_land_service {
  export MULTI_REPO_LAND_SERVICE_PORT
  wait_for_server "Multi-repo land service" MULTI_REPO_LAND_SERVICE_PORT "$TESTTMP/multi_repo_land_service.out" \
    "${MONONOKE_MULTI_REPO_LAND_SERVICE_START_TIMEOUT:-"$MONONOKE_MULTI_REPO_LAND_SERVICE_DEFAULT_START_TIMEOUT"}" "$TESTTMP/multi_repo_land_service_addr.txt" \
    sleep 5
}

function start_and_wait_for_multi_repo_land_service {
  multi_repo_land_service "$@"
  wait_for_multi_repo_land_service
}

function multi_repo_land_service_client {
  THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/client0.crt" \
  THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/client0.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  "$MONONOKE_MULTI_REPO_LAND_SERVICE_CLIENT" \
    --host "$(multi_repo_land_service_address)" \
    "$@"
}

function _megarepo_async_worker_cmd {
  render_mononoke_config
  GLOG_minloglevel=5 \
    RUST_LOG="warm_bookmarks_cache=WARN,dbbookmarks=WARN" \
    "$ASYNC_REQUESTS_WORKER" "$@" \
    --log-level INFO \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --scuba-log-file "$TESTTMP/async-worker.json" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}"
}

function megarepo_async_worker {
  _megarepo_async_worker_cmd "$@" >> "$TESTTMP/megarepo_async_worker.out" 2>&1 &
  export MEGAREPO_ASYNC_WORKER_PID=$!
  echo "$MEGAREPO_ASYNC_WORKER_PID" >> "$DAEMON_PIDS"
}

function megarepo_async_worker_foreground {
  _megarepo_async_worker_cmd "$@"
}

function backfill_worker {
  render_mononoke_config
  GLOG_minloglevel=5 \
    RUST_LOG="warm_bookmarks_cache=WARN,dbbookmarks=WARN" \
    "$BACKFILL_WORKER" "$@" \
    --log-level INFO \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --scuba-log-file "$TESTTMP/async-worker.json" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    >> "$TESTTMP/backfill_worker.out" 2>&1 &
  export BACKFILL_WORKER_PID=$!
  echo "$BACKFILL_WORKER_PID" >> "$DAEMON_PIDS"
}

function scsc_as {
  local name="$1"
  shift
  GLOG_minloglevel=5 \
    THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/$name.crt" \
    THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/$name.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    "$SCS_CLIENT" --host "$LOCALIP:$SCS_PORT" "$@"
}

function scsc {
  scsc_as client0 "$@"
}

function scsc_scmquery_test {
  local jk_args=()
  if [[ -n "$MONONOKE_JUST_KNOBS_OVERRIDES_PATH" ]]; then
    jk_args=(--just-knobs-config-path "$MONONOKE_JUST_KNOBS_OVERRIDES_PATH")
  fi
  SCSC_SCMQUERY_TEST_ENABLED=1 scsc \
    "${jk_args[@]}" \
    scmqueryclient-test "$@"
}

# Forces scmqueryclient-rust onto the SCS-direct path by enabling every
# per-method scm/scmquery:client_library_<method>_direct JustKnob in the
# per-test override file (this diff replaced the old shared
# scm/scmquery:direct_scs knob with these per-method knobs).
function enable_scmquery_scs_direct {
  merge_just_knobs <<EOF
{
  "bools": {
    "scm/scmquery:client_library_get_commit_v2_direct": true,
    "scm/scmquery:client_library_cat_v2_direct": true,
    "scm/scmquery:client_library_log_v2_direct": true,
    "scm/scmquery:client_library_get_diff_direct": true,
    "scm/scmquery:client_library_is_ancestor_direct": true,
    "scm/scmquery:client_library_translate_revs_direct": true,
    "scm/scmquery:client_library_locate_files_v2_direct": true,
    "scm/scmquery:client_library_get_commits_between_on_path_direct": true,
    "scm/scmquery:client_library_ls_v2_direct": true,
    "scm/scmquery:client_library_get_changed_files_direct": true,
    "scm/scmquery:client_library_merge_base_direct": true,
    "scm/scmquery:client_library_get_generation_direct": true,
    "scm/scmquery:client_library_get_tags_direct": true,
    "scm/scmquery:client_library_get_tags_compact_direct": true,
    "scm/scmquery:client_library_blame_v2_direct": true,
    "scm/scmquery:client_library_get_branches_direct": true,
    "scm/scmquery:client_library_get_metadata_diff_direct": true
  },
  "ints": {
    "scm/scmquery:changed_file_groups_limit": 1000
  }
}
EOF
}

function lfs_health {
  local poll proto bound_addr_file
  poll="$1"; shift
  proto="$1";shift
  bound_addr_file="$1"; shift
  "$poll" "${proto}://$(cat "$bound_addr_file")/health_check"
}

function lfs_server {
  local uri log opts args proto poll lfs_server_pid bound_addr_file lfs_instance instance_count_file
  proto="http"
  poll="curl"

  # Used to separate log files etc when multiple lfs servers started in a test
  instance_count_file="$TESTTMP/lfs_instance_count.txt"

  # lfs_server is started from subshells in the .t tests
  # so use a file rather than env var to keep count
  if [[ ! -r "$instance_count_file" ]]; then
    echo 0 > "$instance_count_file"
  fi

  lfs_instance=$(($(cat "$instance_count_file") + 1))
  echo "$lfs_instance" > "$instance_count_file"

  log="${TESTTMP}/lfs_server.$lfs_instance"

  bound_addr_file="$TESTTMP/lfs_server_addr.$lfs_instance"
  rm -f "$bound_addr_file"

  render_mononoke_config
  opts=(
    "${CACHE_ARGS[@]}"
    "${COMMON_ARGS[@]}"
    --config-tier "$MONONOKE_CONFIG_TIER"
    --listen-port 0
    --bound-address-file "$bound_addr_file"
    --test-friendly-logging
  )
  args=()

  while [[ "$#" -gt 0 ]]; do
    if [[ "$1" = "--upstream" ]]; then
      shift
      args=("${args[@]}" --upstream-url "$1") # Upstream URL is now a named parameter.
      shift
    elif [[ "$1" = "--live-config" ]]; then
      opts=("${opts[@]}" "$1" "$2") # --live-config-fetch-interval is no longer used.
      shift
      shift
    elif [[ "$1" = "--tls" ]]; then
      proto="https"
      poll="sslcurl"
      opts=(
        "${opts[@]}"
        --tls-ca "$TEST_CERTDIR/root-ca.crt"
        --tls-private-key "$TEST_CERTDIR/localhost.key"
        --tls-certificate "$TEST_CERTDIR/localhost.crt"
        --tls-ticket-seeds "$TEST_CERTDIR/server.pem.seeds"
      )
      shift
    elif
      [[ "$1" = "--always-wait-for-upstream" ]] ||
      [[ "$1" = "--readonly" ]] ||
      [[ "$1" = "--git-blob-upload-allowed" ]] ||
      [[ "$1" = "--enable-compression-sniff" ]]
    then
      opts=("${opts[@]}" "$1")
      shift
    elif
      [[ "$1" = "--scuba-dataset" ]] ||
      [[ "$1" = "--scuba-log-file" ]] ||
      [[ "$1" = "--max-upload-size" ]]
    then
      opts=("${opts[@]}" "$1" "$2")
      shift
      shift
    elif [[ "$1" = "--log" ]]; then
      shift
      log="$1"
      shift
    else
      echo "invalid argument: $1" >&2
      return 1
    fi
  done

  if [[ "$proto" = "https" ]]; then
    # need to use localhost to match test certs
    listen_host="localhost"
  else
    # use local IP as its more stable for test expectations when localhost is multihomed
    listen_host="$(mononoke_host)"
  fi
  opts=("${opts[@]}" "--listen-host" "$listen_host")

  GLOG_minloglevel=5 "$LFS_SERVER" \
    "${opts[@]}" "${args[@]}" >> "$log" 2>&1 &

  lfs_server_pid="$!"
  echo "$lfs_server_pid" >> "$DAEMON_PIDS"

  export LFS_PORT
  wait_for_server "lfs_server" "LFS_PORT" "$log" \
    "${MONONOKE_LFS_START_TIMEOUT:-"$MONONOKE_LFS_DEFAULT_START_TIMEOUT"}" "$bound_addr_file" \
    lfs_health "$poll" "$proto" "$bound_addr_file"

  export LFS_HOST_PORT
  export BASE_LFS_URL
  LFS_HOST_PORT="$listen_host:$LFS_PORT"
  BASE_LFS_URL="${proto}://$LFS_HOST_PORT"
  echo "$BASE_LFS_URL"

  cp "$log" "$log.saved"
  truncate -s 0 "$log"
}

function git_client {
  git_client_as "client0" "$@"
}

function git_client_as {
  local name="$1"
  shift
  git -c http.sslCAInfo="$TEST_CERTDIR/root-ca.crt" -c http.sslCert="$TEST_CERTDIR/$name.crt" -c http.sslKey="$TEST_CERTDIR/$name.key" -c push.negotiate=false "$@"
}

function mononoke_git_service {
  # Set Git related environment variables
  local bound_addr_file log
  bound_addr_file="$TESTTMP/mononoke_git_service_addr.txt"
  log="${TESTTMP}/mononoke_git_service.out"
  rm -f "$bound_addr_file"
  render_mononoke_config
  GLOG_minloglevel=5 \
    THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/client0.crt" \
    THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/client0.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    "$MONONOKE_GIT_SERVER" "$@" \
    --tls-ca "$TEST_CERTDIR/root-ca.crt" \
    --tls-private-key "$TEST_CERTDIR/localhost.key" \
    --tls-certificate "$TEST_CERTDIR/localhost.crt" \
    --tls-ticket-seeds "$TEST_CERTDIR/server.pem.seeds" \
    --listen-port 0 \
    --scuba-log-file "$TESTTMP/scuba.json" \
    --log-level DEBUG \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --bound-address-file "$TESTTMP/mononoke_git_service_addr.txt" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" >> "$log" 2>&1 &
  export MONONOKE_GIT_SERVICE_PID=$!
  echo "$MONONOKE_GIT_SERVICE_PID" >> "$DAEMON_PIDS"

  export MONONOKE_GIT_SERVICE_PORT
  wait_for_server "Mononoke Git Service" "MONONOKE_GIT_SERVICE_PORT" "$log" \
    "${MONONOKE_GIT_SERVICE_START_TIMEOUT:-"$MONONOKE_GIT_SERVICE_DEFAULT_START_TIMEOUT"}" "$bound_addr_file"

  export MONONOKE_GIT_SERVICE_BASE_URL
  MONONOKE_GIT_SERVICE_BASE_URL="https://localhost:$MONONOKE_GIT_SERVICE_PORT/repos/git/ro"
}

function hginit_treemanifest() {
  hg init "$@"
  cat >> "$1"/.hg/hgrc <<EOF
[extensions]
commitextras=

[remotefilelog]
reponame=$1
server=True
EOF
}

function hgmn_init() {
  hg init "$@"
  cat >> "$1"/.hg/hgrc <<EOF
[remotefilelog]
reponame=$1
EOF
}

function enableextension() {
  cat >> .hg/hgrc <<EOF
[extensions]
$1=
EOF
}

function register_hooks {
  cd "$TESTTMP/mononoke-config" || exit 1

  reponame_urlencoded="$(urlencode encode "$REPONAME")"
  HOOKBOOKMARK="${HOOKBOOKMARK:-${MASTER_BOOKMARK:-master_bookmark}}"

  if [[ -z "$HOOKBOOKMARK_REGEX" ]]; then
    HOOKBOOKMARK_ENTRY="name=\"$HOOKBOOKMARK\""
  else
    HOOKBOOKMARK_ENTRY="regex=\"$HOOKBOOKMARK_REGEX\""
  fi

  cat >> "repos/$reponame_urlencoded/server.toml" <<CONFIG
[[bookmarks]]
$HOOKBOOKMARK_ENTRY
CONFIG

  while [[ "$#" -gt 0 ]]; do
    HOOK_NAME="$1"
    shift 1
    EXTRA_CONFIG_DESCRIPTOR=""
    if [[ $# -gt 0 ]]; then
      EXTRA_CONFIG_DESCRIPTOR="$1"
      shift 1
    fi
    register_hook "$HOOK_NAME" "$EXTRA_CONFIG_DESCRIPTOR"
  done


}
# Does all the setup necessary for hook tests
function hook_test_setup() {
  HOOKS_SCUBA_LOGGING_PATH="$TESTTMP/hooks-scuba.json" setup_mononoke_config

  register_hooks "$@"

  setup_common_hg_configs
  cd "$TESTTMP" || exit 1

  cat >> "$HGRCPATH" <<EOF
[ui]
ssh="$DUMMYSSH"
EOF
  MASTER_BOOKMARK_DRAWDAG="${MASTER_BOOKMARK:-master_bookmark}"
  testtool_drawdag -R "$REPONAME" --derive-all <<EOF 2>/dev/null
A-B-C
# bookmark: C $MASTER_BOOKMARK_DRAWDAG
# message: A "a"
# message: B "b"
# message: C "c"
EOF

  start_and_wait_for_mononoke_server

  hg clone -q mono:"$REPONAME" repo2 --noupdate

  cd repo2 || exit 1
  cat >> .hg/hgrc <<EOF
[extensions]
pushrebase=
amend=
EOF
}

function setup_hg_lfs() {
  cat >> .hg/hgrc <<EOF
[lfs]
url=$1
threshold=$2
usercache=$3
[remotefilelog]
lfs=True
EOF
}

function setup_hg_modern_lfs() {
  cat >> .hg/hgrc <<EOF
[remotefilelog]
lfs=True
useruststore=True
getpackversion = 2

[worker]
rustworkers=True

[lfs]
url=$1
threshold=$2
backofftimes=0
EOF
}

function aliasverify() {
  mode="$1"
  alias_type="$2"
  shift 2
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_ALIAS_VERIFY" --repo-id $REPOID \
     "${CACHE_ARGS[@]}" \
     "${COMMON_ARGS[@]}" \
     --config-tier "$MONONOKE_CONFIG_TIER" \
     --alias-type "$alias_type" \
     --mode "$mode" \
     --tracing-test-format \
     "$@"
}

# Without rev
function tglogpnr() {
  hg log -G -T "{node|short} {phase} '{desc}' {bookmarks} {remotenames}" "$@"
}

function mkcommit() {
   echo "$1" > "$1"
   hg add "$1"
   hg ci -m "$1"
}

function mkcommitedenapi() {
   echo "$1" > "$1"
   hg add "$1"
   hg ci -m "$1"
}

function mkgitcommit() {
  echo "$1" > "$1"
  git add "$1"
  git commit -aqm "$1"
}

function showgitrepo() {
  git log --all  --oneline --graph --decorate
}

function createdivergentgitbranches() {
  local branch1
  local branch2
  local current_head
  local changed_file
  local file_content_override

  current_head=$(git rev-parse --abbrev-ref HEAD)
  branch1="$1"
  branch2="$2"
  changed_file="$3"
  file_content_override="$4"

  git branch "$branch1" 2>/dev/null
  git branch "$branch2" 2>/dev/null

  git checkout "$branch1" -q
  echo "${file_content_override:-This is $changed_file on $branch1}" > file1
  git add .
  git commit -qam "Changed $changed_file on $branch1"

  git checkout "$branch2" -q
  echo "${file_content_override:-This is $changed_file on $branch2}" > file1
  git add .
  git commit -qam "Changed $changed_file on $branch2"

  git checkout "$current_head" -q
}

function enable_replay_verification_hook {

cat >> "$TESTTMP"/replayverification.py <<EOF
import bindings, os
def verify_replay(repo, **kwargs):
     EXP_ONTO = "EXPECTED_ONTOBOOK"
     EXP_HEAD = "EXPECTED_REBASEDHEAD"
     expected_book = kwargs.get(EXP_ONTO)
     expected_head = kwargs.get(EXP_HEAD)
     actual_book = kwargs.get("key")
     actual_head = kwargs.get("new")
     allowed_replay_books = (repo.config.get("facebook", "hooks.unbundlereplaybooks") or "").split()
     # If there is a problem with the mononoke -> hg sync job we need a way to
     # quickly disable the replay verification to let unsynced bundles
     # through.
     # Disable this hook by placing a file in the .hg directory.
     io = bindings.io.IO.main()
     if os.path.exists(os.path.join(repo.dot_path, 'REPLAY_BYPASS')):
         # [ReplayVerification] Bypassing check as override file is present
         return 0
     if expected_book is None and expected_head is None:
         # We are allowing non-unbundle-replay pushes to go through
         return 0
     if allowed_replay_books and actual_book not in allowed_replay_books:
         io.write_err(("[ReplayVerification] only allowed to unbundlereplay on %r\n" % (allowed_replay_books, )).encode())
         return 1
     expected_head = expected_head or None
     actual_head = actual_head or None
     if expected_book == actual_book and expected_head == actual_head:
        # [ReplayVerification] Everything seems in order
        return 0

     io.write_err(("[ReplayVerification] Expected: (%r, %r). Actual: (%r, %r)\n" % (expected_book, expected_head, actual_book, actual_head)).encode())
     return 1
EOF

cat >> "$TESTTMP"/repo_lock.py << 'EOF'
import bindings
def run(repo, **kwargs):
   """Repo is locked for everything except replays
   In-process style hook."""
   if kwargs.get("EXPECTED_ONTOBOOK"):
       return 0
   io = bindings.io.IO.main()
   io.write_err(b"[RepoLock] Repo locked for non-unbundlereplay pushes\n")
   return 1
EOF

[[ -f .hg/hgrc ]] || echo ".hg/hgrc does not exists!"

cat >>.hg/hgrc <<CONFIG
[experimental]
run-python-hooks-via-pyhook = true
[hooks]
prepushkey = python:$TESTTMP/replayverification.py:verify_replay
prepushkey.lock = python:$TESTTMP/repo_lock.py:run
CONFIG

}

function add_synced_commit_mapping_entry() {
  local small_repo_id large_repo_id small_bcs_id large_bcs_id version
  small_repo_id="$1"
  small_bcs_id="$2"
  large_repo_id="$3"
  large_bcs_id="$4"
  version="$5"
  quiet mononoke_admin cross-repo --source-repo-id "$small_repo_id" --target-repo-id "$large_repo_id" insert rewritten --source-commit-id "$small_bcs_id" \
    --target-commit-id "$large_bcs_id" \
    --version-name "$version"
}

function crossrepo_verify_bookmarks() {
  local small_repo_id large_repo_id
  small_repo_id="$1"
  shift
  large_repo_id="$1"
  shift
  mononoke_admin cross-repo --source-repo-id "$small_repo_id" --target-repo-id "$large_repo_id" verify-bookmarks "$@"
}

function read_blobstore_wal_queue_size() {
  if [[ -n "$DB_SHARD_NAME" ]]; then
    echo "SELECT COUNT(*) FROM blobstore_write_ahead_log;" | db "$DB_SHARD_NAME" 2> /dev/null | grep -v COUNT
  else
    local attempts timeout ret
    timeout="100"
    attempts="$((timeout * 10))"
    for _ in $(seq 1 $attempts); do
      ret="$(sqlite3 "$TESTTMP/blobstore_sync_queue/sqlite_dbs" "select count(*) from blobstore_write_ahead_log" 2>/dev/null)"
      if [[ -n "$ret" ]]; then
        echo "$ret"
        return 0
      fi
      sleep 0.1
    done
    return 1
  fi

}

function log() {
  # Prepend "$" to the end of the log output to prevent having trailing whitespaces
  hg log -G -T "{desc} [{phase};rev={rev};{node|short}] {remotenames}" "$@" | sed 's/^[ \t]*$/$/'
}

function log_globalrev() {
  # Prepend "$" to the end of the log output to prevent having trailing whitespaces
  hg log -G -T "{desc} [{phase};globalrev={globalrev};{node|short}] {remotenames}" "$@" | sed 's/^[ \t]*$/$/'
}

function default_setup() {
  default_setup_drawdag "$@"
  echo "starting Mononoke"

  start_and_wait_for_mononoke_server "$@"

  echo "cloning repo in hg client 'repo2'"
  hg clone -q "mono:$REPONAME" repo2 --noupdate
  cd repo2 || exit 1
  cat >> .hg/hgrc <<EOF
[extensions]
pushrebase =
EOF
}

function default_setup_drawdag() {
  setup_common_config "$BLOB_TYPE"

  cd "$TESTTMP" || exit 1

  testtool_drawdag -R repo --derive-all <<EOF
C
|
B
|
A
# bookmark: C "${MASTER_BOOKMARK:-master_bookmark}"
EOF

  start_and_wait_for_mononoke_server "$@"
  hg clone -q "mono:repo" "$REPONAME" --noupdate
  cd $REPONAME || exit 1
  cat >> .hg/hgrc <<EOF
[ui]
ssh ="$DUMMYSSH"
[extensions]
amend =
pushrebase =
EOF
}

function gitexport() {
  log="$TESTTMP/gitexport.out"

  render_mononoke_config
  "$MONONOKE_GITEXPORT" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    "$@"
}

function gitimport() {
  local git_cmd
  # git.real not present in OSS but mononoke defaults to it, so detect right command
  if type --path git.real > /dev/null; then
    git_cmd="git.real"
  else
    git_cmd="git"
  fi

  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_GITIMPORT" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --git-command-path $git_cmd\
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tls-ca "$TEST_CERTDIR/root-ca.crt" \
    --tls-private-key "$TEST_CERTDIR/client0.key" \
    --tls-certificate "$TEST_CERTDIR/client0.crt" \
    --tracing-test-format \
    --persist-partial-mappings \
    "$@"
}

function _setup_meta_git_exec_path() {
  # Set up GIT_EXEC_PATH in a test-scoped temp dir with all helpers from
  # the Meta-built git. Uses $TESTTMP to avoid mutating the shared Buck
  # output directory. Called once per test; symlinks persist for the run.
  local exec_path="$1"
  mkdir -p "$exec_path"
  # Main git binary
  ln -sf "$META_GIT_CLIENT" "$exec_path/git"
  # Remote helpers (ln -sf is idempotent)
  [[ -n "${META_GIT_REMOTE_HTTP:-}" ]] && {
    ln -sf "$META_GIT_REMOTE_HTTP" "$exec_path/git-remote-http"
    ln -sf "$META_GIT_REMOTE_HTTP" "$exec_path/git-remote-https"
    ln -sf "$META_GIT_REMOTE_HTTP" "$exec_path/git-remote-ftp"
    ln -sf "$META_GIT_REMOTE_HTTP" "$exec_path/git-remote-ftps"
  }
  [[ -n "${META_GIT_HTTP_BACKEND:-}" ]] && ln -sf "$META_GIT_HTTP_BACKEND" "$exec_path/git-http-backend"
  [[ -n "${META_GIT_SH_I18N_ENVSUBST:-}" ]] && ln -sf "$META_GIT_SH_I18N_ENVSUBST" "$exec_path/git-sh-i18n--envsubst"
  # Shell scripts
  [[ -n "${META_GIT_SH_SETUP:-}" ]] && ln -sf "$META_GIT_SH_SETUP" "$exec_path/git-sh-setup"
  [[ -n "${META_GIT_SH_I18N:-}" ]] && ln -sf "$META_GIT_SH_I18N" "$exec_path/git-sh-i18n"
  [[ -n "${META_GIT_SUBMODULE:-}" ]] && ln -sf "$META_GIT_SUBMODULE" "$exec_path/git-submodule"
  # Also symlink git-lfs from system PATH so LFS operations work
  local system_lfs
  system_lfs="$(type -P git-lfs 2>/dev/null || true)"
  [[ -n "$system_lfs" ]] && ln -sf "$system_lfs" "$exec_path/git-lfs"
}

# Set up Meta git exec path once when library.sh is sourced.
# $TESTTMP is always set by the test runner before library.sh is sourced.
_META_GIT_EXEC_PATH=""
if [[ -n "${META_GIT_CLIENT:-}" && -x "${META_GIT_CLIENT}" ]]; then
  _META_GIT_EXEC_PATH="${TESTTMP}/meta-git-exec"
  _setup_meta_git_exec_path "$_META_GIT_EXEC_PATH"
  export PATH="${_META_GIT_EXEC_PATH}:${PATH}"
  export GIT_TERMINAL_PROMPT=0
fi

function git() {
  local date name email
  date="01/01/0000 00:00 +0000"
  name="mononoke"
  email="mononoke@mononoke"

  local git_bin="git"
  if [[ -n "${META_GIT_CLIENT:-}" && -x "${META_GIT_CLIENT}" ]]; then
    git_bin="$META_GIT_CLIENT"
  fi

  if ! command "$git_bin" config --get uploadpack.allowFilter > /dev/null; then
    GIT_AUTHOR_NAME="$name" \
    GIT_AUTHOR_EMAIL="$email" \
    command "$git_bin" config --global uploadpack.allowFilter true > /dev/null
  fi

  if [[ -n "$_META_GIT_EXEC_PATH" ]]; then
    GIT_EXEC_PATH="$_META_GIT_EXEC_PATH" \
    GIT_COMMITTER_DATE="${GIT_COMMITTER_DATE:-$date}" \
    GIT_COMMITTER_NAME="$name" \
    GIT_COMMITTER_EMAIL="$email" \
    GIT_AUTHOR_DATE="${GIT_AUTHOR_DATE:-$date}" \
    GIT_AUTHOR_NAME="$name" \
    GIT_AUTHOR_EMAIL="$email" \
    command "$git_bin" -c transfer.bundleURI=false -c init.defaultBranch=master_bookmark -c protocol.file.allow=always -c commit.recordPredecessor=true "$@"
  else
    GIT_COMMITTER_DATE="${GIT_COMMITTER_DATE:-$date}" \
    GIT_COMMITTER_NAME="$name" \
    GIT_COMMITTER_EMAIL="$email" \
    GIT_AUTHOR_DATE="${GIT_AUTHOR_DATE:-$date}" \
    GIT_AUTHOR_NAME="$name" \
    GIT_AUTHOR_EMAIL="$email" \
    command git -c transfer.bundleURI=false -c init.defaultBranch=master_bookmark -c protocol.file.allow=always -c commit.recordPredecessor=false "$@"
  fi
}

function git_set_only_author() {
  local date name email
  date="01/01/0000 00:00 +0000"
  name="mononoke"
  email="mononoke@mononoke"
  local git_bin="git"
  if [[ -n "${META_GIT_CLIENT:-}" && -x "${META_GIT_CLIENT}" ]]; then
    git_bin="$META_GIT_CLIENT"
  fi
  if [[ -n "$_META_GIT_EXEC_PATH" ]]; then
    GIT_EXEC_PATH="$_META_GIT_EXEC_PATH" \
    GIT_AUTHOR_DATE="$date" \
    GIT_AUTHOR_NAME="$name" \
    GIT_AUTHOR_EMAIL="$email" \
    command "$git_bin" -c init.defaultBranch=master_bookmark -c protocol.file.allow=always -c commit.recordPredecessor=true "$@"
  else
    GIT_AUTHOR_DATE="$date" \
    GIT_AUTHOR_NAME="$name" \
    GIT_AUTHOR_EMAIL="$email" \
    command git -c init.defaultBranch=master_bookmark -c protocol.file.allow=always -c commit.recordPredecessor=false "$@"
  fi
}

function summarize_scuba_json() {
  local interesting_tags
  local key_spec
  interesting_tags="$1"
  shift
  key_spec=""
  for key in "$@"
  do
     key_spec="$key_spec + (if (${key} != null) then {${key##*.}: ${key}} else {} end)"
  done
  jq -S "if (.normal.log_tag | match(\"^($interesting_tags)\$\")) then ${key_spec:3} else empty end"
}

if [ -z "$HAS_FB" ]; then
  function format_single_scuba_sample() {
    jq -S .
  }

  function format_single_scuba_sample_strip_server_info {
      jq -S 'del(.[].server_tier, .[].tw_task_id, .[].tw_handle, .[].datacenter, .[].region, .[].region_datacenter_prefix, .normvector.enabled_experiments_jk, .normvector.use_maybe_stale_freshness_for_bookmarks)'
  }
fi

function microwave_builder() {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_MICROWAVE_BUILDER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    "$@"
}

function derivation_pipeline_tailer {
  render_mononoke_config
  THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/client0.crt" \
  THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/client0.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  GLOG_minloglevel=5 "$DERIVATION_PIPELINE_TAILER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    "$@"
}

function start_and_wait_for_derivation_pipeline_worker {
  local addr_file="$TESTTMP/pipeline_worker.addr"
  rm -f "$addr_file"
  render_mononoke_config
  THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
  THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
  GLOG_minloglevel=5 "$DERIVATION_PIPELINE_WORKER" \
    --port 0 \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --bound-address-file "$addr_file" \
    --scuba-log-file "$TESTTMP/pipeline_worker_scuba.json" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" "${COMMON_ARGS[@]}" "$@" \
    >> "$TESTTMP/pipeline_worker.out" 2>&1 &
  echo "$!" >> "$DAEMON_PIDS"
  export PIPELINE_WORKER_PORT
  wait_for_server "Derivation pipeline worker" PIPELINE_WORKER_PORT \
    "$TESTTMP/pipeline_worker.out" "$MONONOKE_DDS_DEFAULT_START_TIMEOUT" \
    "$addr_file" derivation_pipeline_worker_health
}

function derivation_pipeline_worker_health {
  DDS_PORT="$PIPELINE_WORKER_PORT" derived_data_client get-status
}

function derived_data_tailer {
  render_mononoke_config
  GLOG_minloglevel=5 "$DERIVED_DATA_TAILER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function manifest_reconciler_job {
  render_mononoke_config
  GLOG_minloglevel=5 "$MANIFEST_RECONCILER_JOB" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    "$@"
}

function manifest_tailer_job {
  render_mononoke_config
  GLOG_minloglevel=5 "$MANIFEST_TAILER_JOB" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    "$@"
}

function hook_tailer() {
  render_mononoke_config
  "$MONONOKE_HOOK_TAILER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

# quiet <command> - run command and suppress all output in case of success
#
# This helper function allows to suppress overly verbose logging when there
# are no errors but print all the useful stacktraces when there are errors.
#
# The function always returns the original command return code.
#
# The function behaviour can be tweaked further by tweaking the following env
# variables:
# * QUIET_LOGGING_LOG_FILE - the file where the command output is stored (default: $TESTTMP/quiet.last.log)
# * EXPECTED_RC - the return code that's expected and considered success (default: 0)
function quiet() {
  local log=${QUIET_LOGGING_LOG_FILE:="$TESTTMP/quiet.last.log"}
  "$@" >"$log" 2>&1
  ret="$?"
  expected_ret=${EXPECTED_RC:=0}
  if [[ "$ret" != "$expected_ret" ]]; then
    cat "$log" >&2
  fi
  return "$ret"
}

# quiet_grep <grep_args> -- <command> - run command and suppress all output in case of success
#
# This helper function allows to suppress overly verbose logging when the output
# matches grep expression but display full output otherwise.
#
# Full command output is always printed to stderr.
#
# The function always returns the original command return code (so if you count
# or sort the lines of grepped output) the full output won't be affected.
#
# The function behaviour can be tweaked further by tweaking the following env
# variables:
# * QUIET_LOGGING_LOG_FILE - the file where the command output is stored (default: $TESTTMP/quiet.last.log)
function quiet_grep() {
  ret="$?"
  local log=${QUIET_LOGGING_LOG_FILE:="$TESTTMP/quiet.last.log"}
  GREP_ARGS=()
  while [[ $# -gt 0 ]]; do
    case $1 in
      --)
        shift
        break
        ;;
      *)
        GREP_ARGS+=("$1")
        shift
        ;;
    esac
  done
  "$@" >"$log" 2>&1
  ret="$?"
  if grep "${GREP_ARGS[@]}" < "$log" 2>&1 > /dev/null; then
    grep "${GREP_ARGS[@]}" < "$log"
  else
    cat "$log" >&2
  fi
  return "$ret"
}

function streaming_clone() {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_STREAMING_CLONE" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function repo_import() {
  log="$TESTTMP/repo_import.out"

  local git_cmd
  # git.real not present in OSS but mononoke defaults to it, so detect right command
  if type --path git.real > /dev/null; then
    git_cmd="git.real"
  else
    git_cmd="git"
  fi

  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_REPO_IMPORT" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --git-command-path "$git_cmd"\
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function sqlite3() {
  # Set a longer timeout so that we don't break if Mononoke currently has a
  # handle on the DB.
  command sqlite3 -cmd '.timeout 1000' "$@"
}

function merge_just_knobs() {
  local new
  new="$(jq -s '.[0] * .[1]' "$MONONOKE_JUST_KNOBS_OVERRIDES_PATH" -)" || {
    echo "merge_just_knobs: jq failed to merge JustKnobs overrides" >&2
    return 1
  }
  printf "%s" "$new" > "$MONONOKE_JUST_KNOBS_OVERRIDES_PATH"
}

function packer() {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_PACKER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function check_git_wc() {
  render_mononoke_config
  GLOG_minloglevel=5 "$MONONOKE_CHECK_GIT_WC" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --repo-id "$REPOID" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

function derived_data_service() {
  export DDS_PORT

  rm -rf "$DDS_SERVER_ADDR_FILE"

  render_mononoke_config
  THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
  THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
  GLOG_minloglevel=5 "$DERIVED_DATA_SERVICE" "$@" \
    -p 0 \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --bound-address-file "$DDS_SERVER_ADDR_FILE" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" >> "$TESTTMP/derived_data_service.out" 2>&1 &

  pid=$!
  echo "$pid" >> "$DAEMON_PIDS"
}

function start_and_wait_for_dds() {
  derived_data_service "$@"
  wait_for_dds
}

function wait_for_dds() {
  export DDS_PORT
  wait_for_server "Derived data service" DDS_PORT "$TESTTMP/derived_data_service.out" \
    "${MONONOKE_DDS_START_TIMEOUT:-"$MONONOKE_DDS_DEFAULT_START_TIMEOUT"}" "$DDS_SERVER_ADDR_FILE" \
    derived_data_client get-status
}

function derived_data_client() {
  GLOG_minloglevel=5 \
  THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/client0.crt" \
  THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/client0.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  GLOG_minloglevel=5 "$DERIVED_DATA_CLIENT" \
  --host "localhost:$DDS_PORT" \
  "$@"
}

function derivation_worker() {
  render_mononoke_config
  GLOG_minloglevel=5 "$DERIVED_DATA_WORKER" \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --tracing-test-format \
    "$@"
}

DIFF_SERVICE_SERVER_ADDR_FILE="$TESTTMP/diff_service_server_addr.txt"
MONONOKE_DIFF_SERVICE_DEFAULT_START_TIMEOUT=60
function diff_service() {
    export DIFF_SERVICE_PORT

    rm -rf "$DIFF_SERVICE_SERVER_ADDR_FILE"

    render_mononoke_config
    THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
    THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
    THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/proxy.crt" \
    THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/proxy.key" \
    "$MONONOKE_DIFF_SERVICE_SERVER" "$@" \
      -p 0 \
      --config-tier "$MONONOKE_CONFIG_TIER" \
      --scuba-log-file "$TESTTMP/diff-service.json" \
      --bound-address-file "$DIFF_SERVICE_SERVER_ADDR_FILE" \
      --tracing-test-format \
      "${CACHE_ARGS[@]}" \
      "${COMMON_ARGS[@]}" >> "$TESTTMP/diff_service.out" 2>&1 &

    pid=$!
    export DIFF_SERVICE_PID=$!
    echo "$pid" >> "$DAEMON_PIDS"
}

function start_and_wait_for_diff_service() {
    diff_service "$@"
    wait_for_diff_service
}

function diff_service_client() {
    diff_service_client_as client0 "$@"
}

function diff_service_client_as() {
    local name="$1"
    shift
    THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/$name.crt" \
    THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/$name.key" \
    THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
    $MONONOKE_DIFF_SERVICE_CLIENT \
      --host "localhost:$DIFF_SERVICE_PORT" \
      --tracing-test-format \
      "$@"
}

function wait_for_diff_service() {
    export DIFF_SERVICE_PORT
    wait_for_server "Diff service" DIFF_SERVICE_PORT "$TESTTMP/diff_service.out" \
      "${MONONOKE_DIFF_SERVICE_START_TIMEOUT:-"$MONONOKE_DIFF_SERVICE_DEFAULT_START_TIMEOUT"}" "$DIFF_SERVICE_SERVER_ADDR_FILE" \
      diff_service_client status
}

function diff_service_address() {
  tr -d '\n' < "$DIFF_SERVICE_SERVER_ADDR_FILE"
}

BOOKMARK_SERVICE_SERVER_ADDR_FILE="$TESTTMP/bookmark_service_server_addr.txt"
MONONOKE_BOOKMARK_SERVICE_DEFAULT_START_TIMEOUT=60
function bookmark_service() {
  export BOOKMARK_SERVICE_PORT

  rm -rf "$BOOKMARK_SERVICE_SERVER_ADDR_FILE"

  render_mononoke_config
  THRIFT_TLS_SRV_CERT="$TEST_CERTDIR/localhost.crt" \
  THRIFT_TLS_SRV_KEY="$TEST_CERTDIR/localhost.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  THRIFT_TLS_TICKETS="$TEST_CERTDIR/server.pem.seeds" \
  THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/proxy.crt" \
  THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/proxy.key" \
  "$MONONOKE_BOOKMARK_SERVICE_SERVER" "$@" \
    -p 0 \
    --config-tier "$MONONOKE_CONFIG_TIER" \
    --scuba-log-file "$TESTTMP/bookmark-service.json" \
    --bound-address-file "$BOOKMARK_SERVICE_SERVER_ADDR_FILE" \
    --tracing-test-format \
    "${CACHE_ARGS[@]}" \
    "${COMMON_ARGS[@]}" >> "$TESTTMP/bookmark_service.out" 2>&1 &

  pid=$!
  export BOOKMARK_SERVICE_PID=$!
  echo "$pid" >> "$DAEMON_PIDS"
}

function start_and_wait_for_bookmark_service() {
  bookmark_service "$@"
  wait_for_bookmark_service
}

function bookmark_service_client() {
  THRIFT_TLS_CL_CERT_PATH="$TEST_CERTDIR/client0.crt" \
  THRIFT_TLS_CL_KEY_PATH="$TEST_CERTDIR/client0.key" \
  THRIFT_TLS_CL_CA_PATH="$TEST_CERTDIR/root-ca.crt" \
  $MONONOKE_BOOKMARK_SERVICE_CLIENT \
    --host "localhost:$BOOKMARK_SERVICE_PORT" \
    --tracing-test-format \
    "$@"
}

function wait_for_bookmark_service() {
  export BOOKMARK_SERVICE_PORT
  wait_for_server "Bookmark service" BOOKMARK_SERVICE_PORT "$TESTTMP/bookmark_service.out" \
    "${MONONOKE_BOOKMARK_SERVICE_START_TIMEOUT:-"$MONONOKE_BOOKMARK_SERVICE_DEFAULT_START_TIMEOUT"}" "$BOOKMARK_SERVICE_SERVER_ADDR_FILE" \
  bookmark_service_client status
}

# Because of warm bookmark cache the SCS bookmark updates are async.
# This function allows to wait for them.
function wait_for_bookmark_service_update() {
  local repo="$1"
  local bookmark="$2"
  local target="$3"
  local attempt=1
  sleep 2
  while [[ "$(bookmark_service_client get -R "$repo" "$bookmark")" != "$target" ]]
  do
    attempt=$((attempt + 1))
    if [[ $attempt -gt 5 ]]
    then
        echo "bookmark move of $bookmark to $target has not happened"
        return 1
    fi
    sleep 2
  done
}
function verify_integrity_service_health() {
  $THRIFTDBG sendRequest getStatus "{}" --host "localhost" --port "$VI_SERVICE_PORT"
}

function verify_integrity_service() {
  export VI_SERVICE_PORT
  local VI_SERVICE_ADDR_FILE
  VI_SERVICE_ADDR_FILE="$TESTTMP/verify_integrity_service_addr.txt"
  "$VERIFY_INTEGRITY_SERVICE" "$@" \
    --service.port 0 \
    --bound-address-file "$VI_SERVICE_ADDR_FILE" \
    >> "$TESTTMP/verify_integrity_service.out" 2>&1 &

  pid=$!
  echo "$pid" >> "$DAEMON_PIDS"

  wait_for_server "Verify Integrity service" VI_SERVICE_PORT "$TESTTMP/verify_integrity_service.out" \
    "${VI_SERVICE_START_TIMEOUT:-"$VI_SERVICE_DEFAULT_START_TIMEOUT"}" "$VI_SERVICE_ADDR_FILE" \
    verify_integrity_service_health
}

# Wrapper for drawdag that loads all the commit aliases to env variables
# so they can be used to refer to commits instead of hashes.
function testtool_drawdag() {
  out="$(mononoke_testtool drawdag "$@" | tee /dev/fd/2)"
  rc="${PIPESTATUS[0]}"
  # shellcheck disable=SC2046,SC2163,SC2086
  export $out
  return "$rc"
}

function start_zelos_server() {
  PORT=$1
  rm -f "$TESTTMP/local-zelos"
  "$ZELOSCLI" --x server "$PORT" "$TESTTMP/local-zelos" > /dev/null 2>&1 &
  pid=$!
  echo "$pid" >> "$DAEMON_PIDS"
}

function zeloscli() {
  PORT=$1
  shift
  "$ZELOSCLI" --server localhost:"$PORT" -x "$@" 2>/dev/null
}

function x_repo_lookup() {
  SOURCE_REPO="$1"
  TARGET_REPO="$2"
  HASH="$3"
  LOOKUP_BEHAVIOR="${4:-}"
  if [ -z "$LOOKUP_BEHAVIOR" ]; then
    TRANSLATED=$(hg debugapi -e committranslateids -i "[{'Hg': '$HASH'}]" -i "'Hg'" -i "'$SOURCE_REPO'" -i "'$TARGET_REPO'")
  elif [ "$LOOKUP_BEHAVIOR" = "None" ]; then
    TRANSLATED=$(hg debugapi -e committranslateids -i "[{'Hg': '$HASH'}]" -i "'Hg'" -i "'$SOURCE_REPO'" -i "'$TARGET_REPO'" -i None)
  else
    TRANSLATED=$(hg debugapi -e committranslateids -i "[{'Hg': '$HASH'}]" -i "'Hg'" -i "'$SOURCE_REPO'" -i "'$TARGET_REPO'" -i "'$LOOKUP_BEHAVIOR'")
  fi
  if [ -n "$TRANSLATED" ] && [ "${TRANSLATED}" != "[]" ]; then
    hg debugshell <<EOF
print(hex(${TRANSLATED}[0]["translated"]["Hg"]))
EOF
  else
    echo "[]"
  fi
}

function async_worker_enqueue() {
  local repo_id bookmark request_type args_blobstore_key
  repo_id=$1
  bookmark=$2
  request_type=$3
  args_blobstore_key=$4
  status=${5:-new}

  sqlite3 "$TESTTMP/monsql/sqlite_dbs" << EOF
insert into long_running_request_queue
  (repo_id, bookmark, request_type, args_blobstore_key, created_at, inprogress_last_updated_at, status)
values
  ($repo_id, '$bookmark', '$request_type', '$args_blobstore_key', strftime('%s', 'now') * 1000000000, strftime('%s', 'now') * 1000000000, '$status');
EOF
}

function async_requests_clear_queue() {
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" 'delete from long_running_request_queue;'
}

# Wait for bookmark to move to a commit with a certain title
function wait_for_bookmark_move_to_commit {
  local commit_title=$1
  local repo=$2
  local bookmark=${3-master_bookmark}



  local attempts=150
  for _ in $(seq 1 $attempts); do
    mononoke_admin fetch -R "$repo" -B "$bookmark" | rg -q "$commit_title" && return
    sleep 0.1
  done

  echo "bookmark didn't move to commit $commit_title" >&2
  exit 1

}

function fb303-status() {
  $FB303_STATUS "$@"
}
