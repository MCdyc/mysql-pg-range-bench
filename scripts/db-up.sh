#!/usr/bin/env bash

set +x
set -Eeuo pipefail
umask 077

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=linux/local-instance-lib.sh
source "${SCRIPT_DIR}/linux/local-instance-lib.sh"

target="${1:-both}"
case "${target}" in
    mysql)
        services=(mysql)
        ;;
    postgres)
        services=(postgres)
        ;;
    both)
        services=(mysql postgres)
        ;;
    *)
        printf 'Usage: %s [mysql|postgres|both]\n' "$0" >&2
        exit 2
        ;;
esac

local_initialize
local_ensure_instances "${services[@]}"
local_compose ps
printf 'Reusable data directory: %s\n' "${LOCAL_DB_DATA_ROOT}"
