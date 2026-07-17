#!/usr/bin/env bash

set -Eeuo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

assume_yes=false
target=both
for arg in "$@"; do
    case "${arg}" in
        --yes)
            assume_yes=true
            ;;
        mysql|postgres|both)
            target="${arg}"
            ;;
        *)
            printf 'Usage: %s [--yes] [mysql|postgres|both]\n' "$0" >&2
            exit 2
            ;;
    esac
done

delete_args=()
if [[ "${assume_yes}" == true ]]; then
    delete_args+=(--yes)
fi

bash "${PROJECT_ROOT}/scripts/linux/delete-local-instances.sh" "${delete_args[@]}"
bash "${PROJECT_ROOT}/scripts/db-up.sh" "${target}"
