#!/usr/bin/env bash

set -Eeuo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly ONE_CLICK_SCRIPT="${SCRIPT_DIR}/run-one-click.sh"

usage() {
    cat <<'USAGE'
Usage:
  bash scripts/linux/run-query-ten-different.sh [OPTIONS]

This project-local one-click entry point:
  1. inserts all configured rows into each database;
  2. waits for insertion to finish;
  3. performs zero warm-up queries;
  4. executes ten different indexed range COUNT(*) queries;
  5. spreads their start rows deterministically from the first possible range
     to the last possible range, with the same exact row count in every range;
  6. prints and records every query range, count, and elapsed time;
  7. deletes the temporary test databases while keeping reusable instances.

Defaults:
  30,000,000 inserted rows per database
  ten different ranges, each containing exactly 5,000,000 rows

The ranges overlap when ten times --scan-rows exceeds --rows, but all ten
range predicates are different and reproducible.

Smoke example:
  bash scripts/linux/run-query-ten-different.sh --smoke
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
        --skip-insert|--skip-insert=*|--skip-maintenance|--skip-maintenance=*|--skip-lock-test|--skip-lock-test=*|--warmups|--warmups=*|--runs|--runs=*|--query-ranges|--query-ranges=*|--range-start-row|--range-start-row=*)
            die 'query lifecycle, zero warmups, range mode, and ten runs are fixed by this script'
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
    --runs 10 \
    --query-ranges different \
    "${FORWARD_ARGS[@]}"
