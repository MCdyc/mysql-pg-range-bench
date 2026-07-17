#!/usr/bin/env bash

# Prevent inherited tracing from exposing the project-local credential file.
set +x
set -Eeuo pipefail
umask 077

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=local-instance-lib.sh
source "${SCRIPT_DIR}/local-instance-lib.sh"

assume_yes=false
case "${1:-}" in
    '')
        ;;
    --yes)
        assume_yes=true
        ;;
    *)
        printf 'Usage: %s [--yes]\n' "$0" >&2
        exit 2
        ;;
esac

local_initialize_for_delete

if [[ "${assume_yes}" != true ]]; then
    printf '%s\n' \
        'This permanently stops and deletes only this project local instances:' \
        "  ${LOCAL_DB_DATA_ROOT}/mysql" \
        "  ${LOCAL_DB_DATA_ROOT}/postgres" \
        "  ${LOCAL_CREDENTIALS_FILE}"
    read -r -p 'Continue? [y/N] ' answer
    [[ "${answer}" == y || "${answer}" == Y ]] || exit 0
fi

printf '%s\n' 'Stopping project-local MySQL and PostgreSQL instances...'
local_stop_instances ||
    local_die 'could not verify that all project-local containers were removed'

printf 'Deleting project-local instance data under %s\n' "${LOCAL_DB_DATA_ROOT}"
local_delete_instance_files
printf '%s\n' 'Project-local database instances were completely removed.'
