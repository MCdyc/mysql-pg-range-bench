#!/usr/bin/env bash

set -Eeuo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly ONE_CLICK_SCRIPT="${SCRIPT_DIR}/run-one-click.sh"

usage() {
    cat <<'USAGE'
Usage:
  bash scripts/linux/run-query-once.sh [OPTIONS]

This project-local one-click entry point:
  1. starts or reuses the isolated MySQL/PostgreSQL instances;
  2. creates the benchmark databases and inserts all configured rows;
  3. waits for insertion to finish;
  4. skips ANALYZE/VACUUM and warm-up queries;
  5. executes exactly one measured range COUNT(*) per database;
  6. skips the separate SKIP LOCKED test;
  7. deletes and verifies the temporary test databases while keeping the
     reusable project-local instances.

Defaults:
  30,000,000 inserted rows per database
  one 5,000,000-row range COUNT(*) per database

Smoke example:
  bash scripts/linux/run-query-once.sh --smoke

All non-reserved options are forwarded to run-one-click.sh. The fixed
single-query settings cannot be overridden.
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

FORWARD_ARGS=()
while (( $# > 0 )); do
    case "$1" in
        --wrapper-help)
            usage
            exit 0
            ;;
        --skip-insert|--skip-insert=*|--skip-maintenance|--skip-maintenance=*|--skip-lock-test|--skip-lock-test=*|--warmups|--warmups=*|--runs|--runs=*)
            die 'insert/query lifecycle and query count are fixed by this script'
            ;;
        *)
            FORWARD_ARGS+=("$1")
            shift
            ;;
    esac
done

exec bash "${ONE_CLICK_SCRIPT}" \
    --skip-maintenance \
    --skip-lock-test \
    --warmups 0 \
    --runs 1 \
    "${FORWARD_ARGS[@]}"
