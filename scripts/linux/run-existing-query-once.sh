#!/usr/bin/env bash

# Never expose connection URLs through an inherited `bash -x` trace.
set +x
set -Eeuo pipefail
umask 077

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
readonly BINARY="${PROJECT_ROOT}/target/release/mysql-pg-range-bench"

DATABASE_MODE="${BENCH_DATABASE:-both}"
HAS_OUTPUT=0
SKIP_BUILD=0
FORWARD_ARGS=()

usage() {
    cat <<'USAGE'
Usage:
  MYSQL_URL='mysql://...' \
  POSTGRES_URL='postgres://...' \
  bash scripts/linux/run-existing-query-once.sh [OPTIONS]

This read-only entry point expects benchmark_events to be populated already.
For every selected database it performs:
  - one non-ANALYZE JSON EXPLAIN;
  - zero warm-up range queries;
  - exactly one measured range COUNT(*).

It does not insert, run ANALYZE/VACUUM, execute SKIP LOCKED, delete data, or
manage database instances. Control database and Linux page-cache state before
launching this script. For a strict cold-cache experiment, run only one
database per invocation and reset the cache again before the other database.
Build first, then reset caches, then pass --no-build so compilation cannot
disturb the controlled cache state.

Defaults:
  --database both
  --rows 30000000
  --scan-rows 5000000

Connection URLs are accepted only through MYSQL_URL and POSTGRES_URL. They are
never printed or placed in command-line arguments.
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

validate_database_mode() {
    case "$1" in
        both|mysql|postgres|pg) ;;
        *) die '--database must be one of: both, mysql, postgres, pg' ;;
    esac
}

while (( $# > 0 )); do
    case "$1" in
        --wrapper-help)
            usage
            exit 0
            ;;
        --no-build)
            SKIP_BUILD=1
            shift
            ;;
        --database)
            (( $# >= 2 )) || die '--database requires a value'
            [[ "$2" != --* ]] || die '--database requires a value before the next option'
            DATABASE_MODE="$2"
            validate_database_mode "${DATABASE_MODE}"
            FORWARD_ARGS+=("$1" "$2")
            shift 2
            ;;
        --database=*)
            DATABASE_MODE="${1#*=}"
            validate_database_mode "${DATABASE_MODE}"
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
        --mysql-url|--mysql-url=*|--postgres-url|--postgres-url=*)
            die 'connection URLs must be supplied through MYSQL_URL and POSTGRES_URL, not command-line arguments'
            ;;
        --skip-insert|--skip-insert=*|--skip-maintenance|--skip-maintenance=*|--skip-lock-test|--skip-lock-test=*|--warmups|--warmups=*|--runs|--runs=*)
            die 'query count and read-only mode are fixed by this script'
            ;;
        *)
            FORWARD_ARGS+=("$1")
            shift
            ;;
    esac
done

validate_database_mode "${DATABASE_MODE}"
case "${DATABASE_MODE}" in
    both)
        [[ -n "${MYSQL_URL:-}" ]] || die 'MYSQL_URL is required for --database both'
        [[ -n "${POSTGRES_URL:-}" ]] || die 'POSTGRES_URL is required for --database both'
        ;;
    mysql)
        [[ -n "${MYSQL_URL:-}" ]] || die 'MYSQL_URL is required for --database mysql'
        ;;
    postgres|pg)
        [[ -n "${POSTGRES_URL:-}" ]] || die 'POSTGRES_URL is required for --database postgres'
        ;;
esac

[[ "$(uname -s)" == Linux ]] || die 'this entry point must be run on Linux'
if (( SKIP_BUILD == 0 )); then
    command -v cargo >/dev/null 2>&1 ||
        die 'cargo was not found in PATH; install a Rust toolchain first'
fi

cd -- "${PROJECT_ROOT}"
mkdir -p -- "${PROJECT_ROOT}/benchmark-results"

if (( SKIP_BUILD == 0 )); then
    printf 'Building the release query binary from Cargo.lock...\n'
    (
        unset MYSQL_URL POSTGRES_URL
        cargo build --release --locked --bin mysql-pg-range-bench
    )
fi
[[ -x "${BINARY}" ]] || die "expected executable was not built: ${BINARY}"

RUN_STAMP="$(date -u '+%Y%m%dT%H%M%SZ')-$$"
DEFAULT_OUTPUT="benchmark-results/query-once-${RUN_STAMP}.json"
LAUNCH_ARGS=(
    --skip-insert
    --skip-maintenance
    --skip-lock-test
    --warmups 0
    --runs 1
)
if (( HAS_OUTPUT == 0 )); then
    LAUNCH_ARGS+=(--output "${DEFAULT_OUTPUT}")
fi
LAUNCH_ARGS+=("${FORWARD_ARGS[@]}")

printf '%s\n' \
    'Starting read-only query run: zero warmups and one measured COUNT(*) per selected database.' \
    "Result path: $([[ ${HAS_OUTPUT} == 0 ]] && printf '%s' "${DEFAULT_OUTPUT}" || printf '%s' 'user-specified')"

exec "${BINARY}" "${LAUNCH_ARGS[@]}"
