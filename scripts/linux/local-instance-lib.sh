#!/usr/bin/env bash

# Shared lifecycle helpers for the project-local Docker database instances.
# This file must be sourced by another script.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    printf '%s\n' 'error: local-instance-lib.sh must be sourced, not executed' >&2
    exit 2
fi

readonly LOCAL_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly LOCAL_PROJECT_ROOT="$(cd -- "${LOCAL_LIB_DIR}/../.." && pwd -P)"
readonly LOCAL_COMPOSE_FILE="${LOCAL_PROJECT_ROOT}/docker-compose.yml"
readonly LOCAL_DB_BASE="${LOCAL_PROJECT_ROOT}/.local-db"
readonly LOCAL_DB_DATA_ROOT="${LOCAL_DB_BASE}/data"
readonly LOCAL_CREDENTIALS_FILE="${LOCAL_DB_BASE}/credentials.env"
readonly LOCAL_LOCK_FILE="${LOCAL_DB_BASE}/instance.lock"

LOCAL_COMPOSE_PROJECT_NAME=''
LOCAL_MYSQL_ROOT_PASSWORD=''
LOCAL_DB_PASSWORD=''
MYSQL_PORT=''
POSTGRES_PORT=''

local_die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

local_require_runtime() {
    [[ "$(uname -s)" == Linux ]] ||
        local_die 'project-local database instances are supported only on Linux'

    local command_name
    for command_name in docker realpath sha256sum flock od tr awk grep find; do
        command -v "${command_name}" >/dev/null 2>&1 ||
            local_die "required command was not found: ${command_name}"
    done

    docker compose version >/dev/null 2>&1 ||
        local_die "Docker Compose v2 is required (the 'docker compose' command)"
    docker info >/dev/null 2>&1 ||
        local_die 'the Docker daemon is not reachable by the current user'
    local compose_up_help
    compose_up_help="$(docker compose up --help 2>&1)"
    grep -q -- '--wait-timeout' <<<"${compose_up_help}" ||
        local_die "Docker Compose must support 'up --wait-timeout'"
}

local_prepare_base() {
    [[ ! -L "${LOCAL_DB_BASE}" ]] ||
        local_die "${LOCAL_DB_BASE} must not be a symbolic link"
    mkdir -p -- "${LOCAL_DB_BASE}"
    chmod 700 -- "${LOCAL_DB_BASE}"

    local resolved_base
    resolved_base="$(realpath -m -- "${LOCAL_DB_BASE}")"
    [[ "${resolved_base}" == "${LOCAL_PROJECT_ROOT}/.local-db" ]] ||
        local_die 'refusing to use a database directory outside the project'
}

local_acquire_lock() {
    exec 9>"${LOCAL_LOCK_FILE}"
    flock -n 9 ||
        local_die 'another local database benchmark or deletion is already running'
}

local_generate_hex_secret() {
    od -An -N24 -tx1 /dev/urandom | tr -d ' \n'
}

local_read_credential() {
    local key="$1"
    awk -F= -v wanted="${key}" '
        $1 == wanted {
            sub(/^[^=]*=/, "")
            sub(/\r$/, "")
            print
            exit
        }
    ' "${LOCAL_CREDENTIALS_FILE}"
}

local_project_env_value() {
    local key="$1"
    [[ -f "${LOCAL_PROJECT_ROOT}/.env" ]] || return 0
    awk -F= -v wanted="${key}" '
        $1 == wanted {
            sub(/^[^=]*=/, "")
            sub(/\r$/, "")
            print
            exit
        }
    ' "${LOCAL_PROJECT_ROOT}/.env"
}

local_load_or_create_credentials() {
    [[ ! -L "${LOCAL_CREDENTIALS_FILE}" ]] ||
        local_die "${LOCAL_CREDENTIALS_FILE} must not be a symbolic link"

    if [[ ! -e "${LOCAL_CREDENTIALS_FILE}" ]]; then
        local existing_data_entry=''
        if [[ -d "${LOCAL_DB_DATA_ROOT}" ]]; then
            existing_data_entry="$(
                find "${LOCAL_DB_DATA_ROOT}" -mindepth 1 -print -quit
            )"
        fi
        [[ -z "${existing_data_entry}" ]] ||
            local_die 'local instance data exists without its credential file; run delete-local-instances.sh before recreating it'

        local mysql_secret database_secret temporary_file
        mysql_secret="$(local_generate_hex_secret)"
        database_secret="$(local_generate_hex_secret)"
        temporary_file="${LOCAL_CREDENTIALS_FILE}.tmp.$$"
        (
            umask 077
            printf 'LOCAL_MYSQL_ROOT_PASSWORD=%s\n' "${mysql_secret}"
            printf 'LOCAL_DB_PASSWORD=%s\n' "${database_secret}"
        ) >"${temporary_file}"
        chmod 600 -- "${temporary_file}"
        mv -- "${temporary_file}" "${LOCAL_CREDENTIALS_FILE}"
    fi

    [[ -f "${LOCAL_CREDENTIALS_FILE}" ]] ||
        local_die 'local database credential path is not a regular file'
    chmod 600 -- "${LOCAL_CREDENTIALS_FILE}"

    LOCAL_MYSQL_ROOT_PASSWORD="$(
        local_read_credential LOCAL_MYSQL_ROOT_PASSWORD
    )"
    LOCAL_DB_PASSWORD="$(local_read_credential LOCAL_DB_PASSWORD)"
    [[ "${LOCAL_MYSQL_ROOT_PASSWORD}" =~ ^[0-9a-f]{48}$ ]] ||
        local_die 'invalid MySQL secret in the local credential file'
    [[ "${LOCAL_DB_PASSWORD}" =~ ^[0-9a-f]{48}$ ]] ||
        local_die 'invalid database secret in the local credential file'
}

local_validate_port() {
    local name="$1"
    local value="$2"
    [[ "${value}" =~ ^[0-9]+$ ]] ||
        local_die "${name} must be an integer between 1024 and 65535"
    local numeric_value=$((10#${value}))
    (( numeric_value >= 1024 && numeric_value <= 65535 )) ||
        local_die "${name} must be an integer between 1024 and 65535"
}

local_configure_environment() {
    local project_hash configured_mysql_port configured_postgres_port
    local configured_mysql_image configured_postgres_image
    project_hash="$(
        printf '%s' "${LOCAL_PROJECT_ROOT}" |
            sha256sum |
            awk '{ print substr($1, 1, 12) }'
    )"
    LOCAL_COMPOSE_PROJECT_NAME="db-range-benchmark-${project_hash}"

    configured_mysql_port="$(local_project_env_value LOCAL_MYSQL_PORT)"
    configured_postgres_port="$(local_project_env_value LOCAL_POSTGRES_PORT)"
    MYSQL_PORT="${LOCAL_MYSQL_PORT:-${configured_mysql_port:-13306}}"
    POSTGRES_PORT="${LOCAL_POSTGRES_PORT:-${configured_postgres_port:-15432}}"
    local_validate_port LOCAL_MYSQL_PORT "${MYSQL_PORT}"
    local_validate_port LOCAL_POSTGRES_PORT "${POSTGRES_PORT}"
    [[ "${MYSQL_PORT}" != "${POSTGRES_PORT}" ]] ||
        local_die 'MySQL and PostgreSQL local ports must be different'

    export COMPOSE_PROJECT_NAME="${LOCAL_COMPOSE_PROJECT_NAME}"
    export LOCAL_DB_ROOT="${LOCAL_DB_DATA_ROOT}"
    export MYSQL_PORT POSTGRES_PORT
    export DB_NAME='benchmark'
    export DB_USER='benchmark'
    export DB_PASSWORD="${LOCAL_DB_PASSWORD}"
    export MYSQL_ROOT_PASSWORD="${LOCAL_MYSQL_ROOT_PASSWORD}"
    configured_mysql_image="$(local_project_env_value MYSQL_IMAGE)"
    configured_postgres_image="$(local_project_env_value POSTGRES_IMAGE)"
    export MYSQL_IMAGE="${MYSQL_IMAGE:-${configured_mysql_image:-mysql:8.4.8}}"
    export POSTGRES_IMAGE="${POSTGRES_IMAGE:-${configured_postgres_image:-postgres:17.10}}"
}

local_initialize() {
    local_require_runtime
    local_prepare_base
    local_acquire_lock
    local_load_or_create_credentials
    local_configure_environment
}

local_initialize_for_delete() {
    local_require_runtime
    local_prepare_base
    local_acquire_lock
    LOCAL_MYSQL_ROOT_PASSWORD='000000000000000000000000000000000000000000000000'
    LOCAL_DB_PASSWORD='000000000000000000000000000000000000000000000000'
    local_configure_environment
}

local_compose() {
    docker compose \
        --file "${LOCAL_COMPOSE_FILE}" \
        --project-name "${LOCAL_COMPOSE_PROJECT_NAME}" \
        "$@"
}

local_ensure_images() {
    local services=("$@")
    if (( ${#services[@]} == 0 )); then
        services=(mysql postgres)
    fi

    local service image
    for service in "${services[@]}"; do
        case "${service}" in
            mysql)
                image="${MYSQL_IMAGE}"
                ;;
            postgres)
                image="${POSTGRES_IMAGE}"
                ;;
            *)
                local_die "unsupported local database service: ${service}"
                ;;
        esac

        [[ -n "${image}" && "${image}" != -* &&
            "${image}" =~ ^[A-Za-z0-9._/:@-]+$ ]] ||
            local_die "invalid Docker image reference for ${service}"

        if docker image inspect "${image}" >/dev/null 2>&1; then
            printf 'Using cached %s image: %s\n' "${service}" "${image}"
        else
            printf 'Downloading missing %s image: %s\n' "${service}" "${image}"
            docker pull "${image}"
        fi
    done
}

local_ensure_instances() {
    local configured_wait_timeout wait_timeout
    configured_wait_timeout="$(local_project_env_value LOCAL_COMPOSE_WAIT_TIMEOUT)"
    wait_timeout="${LOCAL_COMPOSE_WAIT_TIMEOUT:-${configured_wait_timeout:-300}}"
    local services=("$@")
    if (( ${#services[@]} == 0 )); then
        services=(mysql postgres)
    fi
    [[ "${wait_timeout}" =~ ^[1-9][0-9]*$ ]] ||
        local_die 'LOCAL_COMPOSE_WAIT_TIMEOUT must be a positive integer'

    local_ensure_images "${services[@]}"

    [[ ! -L "${LOCAL_DB_DATA_ROOT}" ]] ||
        local_die "${LOCAL_DB_DATA_ROOT} must not be a symbolic link"
    local database_name database_path resolved_database_path
    for database_name in mysql postgres; do
        database_path="${LOCAL_DB_DATA_ROOT}/${database_name}"
        [[ ! -L "${database_path}" ]] ||
            local_die "${database_path} must not be a symbolic link"
        mkdir -p -- "${database_path}"
        resolved_database_path="$(realpath -m -- "${database_path}")"
        [[ "${resolved_database_path}" == \
            "${LOCAL_PROJECT_ROOT}/.local-db/data/${database_name}" ]] ||
            local_die 'refusing to use a database directory outside the project'
    done

    local_compose up --detach --wait \
        --wait-timeout "${wait_timeout}" \
        "${services[@]}"
}

local_mysql_admin_url() {
    printf 'mysql://root:%s@127.0.0.1:%s/mysql' \
        "${LOCAL_MYSQL_ROOT_PASSWORD}" "${MYSQL_PORT}"
}

local_postgres_admin_url() {
    printf 'postgres://benchmark:%s@127.0.0.1:%s/postgres' \
        "${LOCAL_DB_PASSWORD}" "${POSTGRES_PORT}"
}

local_mysql_cli() {
    local_compose exec -T mysql \
        sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql "$@"' \
        sh "$@"
}

local_postgres_psql() {
    local_compose exec -T postgres \
        sh -c 'PGPASSWORD="$POSTGRES_PASSWORD" exec psql "$@"' \
        sh "$@"
}

local_postgres_dropdb() {
    local_compose exec -T postgres \
        sh -c 'PGPASSWORD="$POSTGRES_PASSWORD" exec dropdb "$@"' \
        sh "$@"
}

local_cleanup_test_databases() {
    local mysql_databases postgres_databases database drop_sql
    local cleanup_failed=0

    if ! mysql_databases="$(
        local_mysql_cli \
            --protocol=tcp --host=127.0.0.1 --user=root \
            --batch --skip-column-names \
            --execute="SELECT schema_name FROM information_schema.schemata WHERE schema_name REGEXP '^codex_range_bench_[0-9a-f]{32}$'"
    )"; then
        printf '%s\n' 'error: could not enumerate local MySQL test databases' >&2
        cleanup_failed=1
        mysql_databases=''
    fi

    while IFS= read -r database; do
        [[ -n "${database}" ]] || continue
        if [[ ! "${database}" =~ ^codex_range_bench_[0-9a-f]{32}$ ]]; then
            printf 'error: refusing unexpected MySQL database name: %s\n' \
                "${database}" >&2
            cleanup_failed=1
            continue
        fi
        drop_sql="$(printf 'DROP DATABASE `%s`' "${database}")"
        local_mysql_cli \
            --protocol=tcp --host=127.0.0.1 --user=root \
            --execute="${drop_sql}" ||
            cleanup_failed=1
    done <<<"${mysql_databases}"

    if ! postgres_databases="$(
        local_postgres_psql \
            --host=127.0.0.1 --username=benchmark \
            --dbname=postgres --tuples-only --no-align \
            --command="SELECT datname FROM pg_catalog.pg_database WHERE datname ~ '^codex_range_bench_[0-9a-f]{32}$'"
    )"; then
        printf '%s\n' 'error: could not enumerate local PostgreSQL test databases' >&2
        cleanup_failed=1
        postgres_databases=''
    fi

    while IFS= read -r database; do
        [[ -n "${database}" ]] || continue
        if [[ ! "${database}" =~ ^codex_range_bench_[0-9a-f]{32}$ ]]; then
            printf 'error: refusing unexpected PostgreSQL database name: %s\n' \
                "${database}" >&2
            cleanup_failed=1
            continue
        fi
        local_postgres_dropdb \
            --force --if-exists --host=127.0.0.1 \
            --username=benchmark "${database}" ||
            cleanup_failed=1
    done <<<"${postgres_databases}"

    local mysql_remaining postgres_remaining
    mysql_remaining="$(
        local_mysql_cli \
            --protocol=tcp --host=127.0.0.1 --user=root \
            --batch --skip-column-names \
            --execute="SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name REGEXP '^codex_range_bench_[0-9a-f]{32}$'"
    )" || cleanup_failed=1
    postgres_remaining="$(
        local_postgres_psql \
            --host=127.0.0.1 --username=benchmark \
            --dbname=postgres --tuples-only --no-align \
            --command="SELECT COUNT(*) FROM pg_catalog.pg_database WHERE datname ~ '^codex_range_bench_[0-9a-f]{32}$'"
    )" || cleanup_failed=1

    [[ "${mysql_remaining//$'\r'/}" == '0' ]] || cleanup_failed=1
    [[ "${postgres_remaining//$'\r'/}" == '0' ]] || cleanup_failed=1

    if (( cleanup_failed != 0 )); then
        printf '%s\n' \
            'error: one or more project-local test databases could not be removed' >&2
        return 1
    fi
    printf '%s\n' \
        'Local test data cleanup verified; reusable database instances were kept.'
}

local_stop_instances() {
    local_compose down --remove-orphans --timeout 60 || true

    local container_output network_output
    container_output="$(
        docker ps --all --quiet \
            --filter "label=com.docker.compose.project=${LOCAL_COMPOSE_PROJECT_NAME}"
    )" || return 1
    if [[ -n "${container_output}" ]]; then
        local container_ids=()
        mapfile -t container_ids <<<"${container_output}"
        docker rm --force "${container_ids[@]}"
    fi

    container_output="$(
        docker ps --all --quiet \
            --filter "label=com.docker.compose.project=${LOCAL_COMPOSE_PROJECT_NAME}"
    )" || return 1
    [[ -z "${container_output}" ]] || return 1

    network_output="$(
        docker network ls --quiet \
            --filter "label=com.docker.compose.project=${LOCAL_COMPOSE_PROJECT_NAME}"
    )" || return 1
    if [[ -n "${network_output}" ]]; then
        local network_ids=()
        mapfile -t network_ids <<<"${network_output}"
        docker network rm "${network_ids[@]}"
    fi

    network_output="$(
        docker network ls --quiet \
            --filter "label=com.docker.compose.project=${LOCAL_COMPOSE_PROJECT_NAME}"
    )" || return 1
    [[ -z "${network_output}" ]]
}

local_delete_instance_files() {
    [[ ! -L "${LOCAL_DB_DATA_ROOT}" ]] ||
        local_die "${LOCAL_DB_DATA_ROOT} must not be a symbolic link"
    local resolved_data
    resolved_data="$(realpath -m -- "${LOCAL_DB_DATA_ROOT}")"
    [[ "${resolved_data}" == "${LOCAL_PROJECT_ROOT}/.local-db/data" ]] ||
        local_die 'refusing to delete a database directory outside the project'

    if [[ -e "${LOCAL_DB_DATA_ROOT}" ]]; then
        chmod -R u+rwX -- "${LOCAL_DB_DATA_ROOT}" 2>/dev/null || true
        rm -rf --one-file-system -- "${LOCAL_DB_DATA_ROOT}" 2>/dev/null || true
    fi

    if [[ -e "${LOCAL_DB_DATA_ROOT}" ]]; then
        docker run --rm --network none \
            --entrypoint sh \
            --volume "${LOCAL_DB_BASE}:/cleanup" \
            "${MYSQL_IMAGE}" \
            -c 'rm -rf -- /cleanup/data'
    fi
    [[ ! -e "${LOCAL_DB_DATA_ROOT}" ]] ||
        local_die 'project-local database data directory could not be removed'

    rm -f -- "${LOCAL_CREDENTIALS_FILE}" "${LOCAL_CREDENTIALS_FILE}.tmp."*
    rm -f -- "${LOCAL_LOCK_FILE}"
    if ! rmdir -- "${LOCAL_DB_BASE}" 2>/dev/null; then
        printf 'warning: %s contains unrelated files and was preserved\n' \
            "${LOCAL_DB_BASE}" >&2
    fi
}
