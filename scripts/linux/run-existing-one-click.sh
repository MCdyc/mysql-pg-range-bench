#!/usr/bin/env bash

# Never expand connection URLs through an inherited `bash -x` trace.
set +x
set -Eeuo pipefail
umask 077

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
readonly BINARY="${PROJECT_ROOT}/target/release/linux-one-click"

DEFAULT_ROWS=30000000
DEFAULT_SCAN_ROWS=5000000
MODE=full
HAS_ROWS=0
HAS_SCAN_ROWS=0
HAS_OUTPUT=0
FORWARD_ARGS=()

usage() {
    cat <<'USAGE'
Usage:
  MYSQL_ADMIN_URL='mysql://...' \
  POSTGRES_ADMIN_URL='postgres://...' \
  bash scripts/linux/run-existing-one-click.sh [OPTIONS]

Default run:
  30,000,000 inserted rows per database and a 5,000,000-row range query.

Smoke run:
  bash scripts/linux/run-existing-one-click.sh --smoke=100k/20k
  The shorthand supplies --rows 100000 --scan-rows 20000.

All other arguments are passed to linux-one-click. In particular, --rows,
--scan-rows, and --output override the wrapper defaults.

Administrator connection URLs are accepted only through MYSQL_ADMIN_URL and
POSTGRES_ADMIN_URL. This wrapper never prints them or places them in argv.
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

on_exit() {
    local status=$?
    if (( status != 0 )); then
        printf '%s\n' \
            "the wrapper exited with status ${status} before handing control to linux-one-click." \
            'This shell wrapper did not create databases or issue cleanup SQL.' >&2
    fi
}
trap on_exit EXIT

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
            die 'administrator URLs must be supplied through MYSQL_ADMIN_URL and POSTGRES_ADMIN_URL, not command-line arguments'
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

[[ "$(uname -s)" == Linux ]] || die 'this entry point must be run on Linux'
command -v cargo >/dev/null 2>&1 || die 'cargo was not found in PATH; install a Rust toolchain first'
[[ -n "${MYSQL_ADMIN_URL:-}" ]] || die 'MYSQL_ADMIN_URL is required'
[[ -n "${POSTGRES_ADMIN_URL:-}" ]] || die 'POSTGRES_ADMIN_URL is required'

cd -- "${PROJECT_ROOT}"
mkdir -p -- "${PROJECT_ROOT}/benchmark-results"

printf 'Building release binaries from Cargo.lock...\n'
(
    unset MYSQL_ADMIN_URL POSTGRES_ADMIN_URL
    cargo build --release --locked --bins
)

[[ -x "${BINARY}" ]] || die "expected executable was not built: ${BINARY}"

RUN_STAMP="$(date -u '+%Y%m%dT%H%M%SZ')-$$"
DEFAULT_OUTPUT="benchmark-results/linux-one-click-${RUN_STAMP}.json"
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

printf 'Starting %s run. Result path: %s\n' "${MODE}" \
    "$([[ ${HAS_OUTPUT} == 0 ]] && printf '%s' "${DEFAULT_OUTPUT}" || printf '%s' 'user-specified')"
exec "${BINARY}" "${LAUNCH_ARGS[@]}"
