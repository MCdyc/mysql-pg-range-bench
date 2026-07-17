#!/usr/bin/env bash

# Prevent inherited `bash -x` from expanding locally stored credentials.
set +x
set -Eeuo pipefail
umask 077

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=local-instance-lib.sh
source "${SCRIPT_DIR}/local-instance-lib.sh"

readonly BINARY="${LOCAL_PROJECT_ROOT}/target/release/linux-one-click"

DEFAULT_ROWS=30000000
DEFAULT_SCAN_ROWS=5000000
MODE=full
HAS_ROWS=0
HAS_SCAN_ROWS=0
HAS_OUTPUT=0
FORWARD_ARGS=()
CHILD_PID=''
FORWARDED_SIGNAL_CODE=0
LOCAL_INSTANCES_READY=0

usage() {
    cat <<'USAGE'
Usage:
  bash scripts/linux/run-one-click.sh [OPTIONS]

This default Linux entry point never connects to a machine-wide database.
It starts or reuses MySQL and PostgreSQL instances whose data lives under:
  .local-db/data/mysql
  .local-db/data/postgres

Missing pinned MySQL/PostgreSQL images are downloaded automatically. On a
blank Linux host, install Docker, Rust and both images first with:
  bash scripts/linux/install.sh

After every run it deletes and verifies all benchmark databases in those local
instances, while keeping the instances available for the next run.

Default run:
  30,000,000 inserted rows per database and a 5,000,000-row range query.

Smoke run:
  bash scripts/linux/run-one-click.sh --smoke=100k/20k

Optional environment:
  LOCAL_MYSQL_PORT=13306
  LOCAL_POSTGRES_PORT=15432
  LOCAL_COMPOSE_WAIT_TIMEOUT=300

To stop and permanently remove the reusable local instances and credentials:
  bash scripts/linux/delete-local-instances.sh
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

while (( $# > 0 )); do
    case "$1" in
        --wrapper-help)
            usage
            exit 0
            ;;
        --smoke|--smoke=100k/20k)
            [[ "${MODE}" == full ]] || die '--smoke may be specified only once'
            MODE=smoke
            DEFAULT_ROWS=100000
            DEFAULT_SCAN_ROWS=20000
            shift
            ;;
        --smoke=*)
            die 'the only supported smoke preset is --smoke=100k/20k'
            ;;
        --rows)
            (( $# >= 2 )) || die '--rows requires a value'
            [[ "$2" != --* ]] || die '--rows requires a value before the next option'
            HAS_ROWS=1
            FORWARD_ARGS+=("$1" "$2")
            shift 2
            ;;
        --rows=*)
            HAS_ROWS=1
            FORWARD_ARGS+=("$1")
            shift
            ;;
        --scan-rows)
            (( $# >= 2 )) || die '--scan-rows requires a value'
            [[ "$2" != --* ]] || die '--scan-rows requires a value before the next option'
            HAS_SCAN_ROWS=1
            FORWARD_ARGS+=("$1" "$2")
            shift 2
            ;;
        --scan-rows=*)
            HAS_SCAN_ROWS=1
            FORWARD_ARGS+=("$1")
            shift
            ;;
        --output)
            (( $# >= 2 )) || die '--output requires a value'
            [[ "$2" != --* ]] || die '--output requires a value before the next option'
            HAS_OUTPUT=1
            FORWARD_ARGS+=("$1" "$2")
            shift 2
            ;;
        --output=*)
            HAS_OUTPUT=1
            FORWARD_ARGS+=("$1")
            shift
            ;;
        --mysql-admin-url|--mysql-admin-url=*|--postgres-admin-url|--postgres-admin-url=*)
            die 'the local entry point does not accept external database URLs'
            ;;
        --database-name|--database-name=*|--mysql-database|--mysql-database=*|--postgres-database|--postgres-database=*|--database-prefix|--database-prefix=*)
            die 'temporary database names are generated internally and cannot be overridden'
            ;;
        *)
            FORWARD_ARGS+=("$1")
            shift
            ;;
    esac
done

forward_signal() {
    local signal_name="$1"
    local exit_code="$2"
    FORWARDED_SIGNAL_CODE="${exit_code}"
    if [[ -n "${CHILD_PID}" ]] && kill -0 "${CHILD_PID}" 2>/dev/null; then
        kill "-${signal_name}" "${CHILD_PID}" 2>/dev/null || true
    else
        exit "${exit_code}"
    fi
}

cleanup_on_exit() {
    local original_status=$?
    trap - EXIT INT TERM HUP
    set +e

    if [[ -n "${CHILD_PID}" ]] && kill -0 "${CHILD_PID}" 2>/dev/null; then
        kill -TERM "${CHILD_PID}" 2>/dev/null || true
        wait "${CHILD_PID}" 2>/dev/null || true
    fi

    local cleanup_status=0
    if (( LOCAL_INSTANCES_READY == 1 )); then
        local_cleanup_test_databases || cleanup_status=$?
    fi

    if (( cleanup_status != 0 )); then
        printf '%s\n' \
            'The reusable local instances were kept, but test-data cleanup failed.' \
            "Inspect ${LOCAL_DB_BASE} and the cleanup receipt before the next run." >&2
        if (( original_status == 0 )); then
            original_status=1
        fi
    fi

    exit "${original_status}"
}

trap 'forward_signal INT 130' INT
trap 'forward_signal TERM 143' TERM
trap 'forward_signal HUP 129' HUP

command -v cargo >/dev/null 2>&1 ||
    die 'cargo was not found in PATH; install a Rust toolchain first'
local_initialize
trap cleanup_on_exit EXIT

cd -- "${LOCAL_PROJECT_ROOT}"
mkdir -p -- "${LOCAL_PROJECT_ROOT}/benchmark-results"

printf 'Building release binaries from Cargo.lock...\n'
(
    unset MYSQL_ADMIN_URL POSTGRES_ADMIN_URL MYSQL_URL POSTGRES_URL
    cargo build --release --locked --bins
)
[[ -x "${BINARY}" ]] || die "expected executable was not built: ${BINARY}"

printf 'Starting or reusing project-local database instances in %s\n' \
    "${LOCAL_DB_DATA_ROOT}"
local_ensure_instances
LOCAL_INSTANCES_READY=1

# Recover test databases left by an uncatchable prior interruption. Prefix
# cleanup is safe here because these instances are dedicated to this project.
local_cleanup_test_databases

RUN_STAMP="$(date -u '+%Y%m%dT%H%M%SZ')-$$"
DEFAULT_OUTPUT="benchmark-results/linux-local-${RUN_STAMP}.json"
LAUNCH_ARGS=()

if (( HAS_ROWS == 0 )); then
    LAUNCH_ARGS+=(--rows "${DEFAULT_ROWS}")
fi
if (( HAS_SCAN_ROWS == 0 )); then
    LAUNCH_ARGS+=(--scan-rows "${DEFAULT_SCAN_ROWS}")
fi
if (( HAS_OUTPUT == 0 )); then
    LAUNCH_ARGS+=(--output "${DEFAULT_OUTPUT}")
fi
LAUNCH_ARGS+=("${FORWARD_ARGS[@]}")

printf 'Starting %s run against project-local instances. Result path: %s\n' \
    "${MODE}" \
    "$([[ ${HAS_OUTPUT} == 0 ]] && printf '%s' "${DEFAULT_OUTPUT}" || printf '%s' 'user-specified')"

MYSQL_ADMIN_URL="$(local_mysql_admin_url)" \
POSTGRES_ADMIN_URL="$(local_postgres_admin_url)" \
    "${BINARY}" "${LAUNCH_ARGS[@]}" &
CHILD_PID=$!

set +e
while true; do
    wait "${CHILD_PID}"
    benchmark_status=$?
    if kill -0 "${CHILD_PID}" 2>/dev/null; then
        continue
    fi
    break
done
set -e
CHILD_PID=''

if (( benchmark_status == 0 && FORWARDED_SIGNAL_CODE != 0 )); then
    benchmark_status="${FORWARDED_SIGNAL_CODE}"
fi
exit "${benchmark_status}"
