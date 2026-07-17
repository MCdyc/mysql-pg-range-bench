#!/usr/bin/env bash

# Install the Linux prerequisites without installing MySQL or PostgreSQL into
# the host operating system. Their pinned Docker images are downloaded instead.
set +x
set -Eeuo pipefail
umask 077

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
readonly RUN_SCRIPT="${SCRIPT_DIR}/run-one-click.sh"

RUN_MODE=install
ROOT_COMMAND=()
TEMP_DIR=''

usage() {
    cat <<'USAGE'
Usage:
  bash scripts/linux/install.sh
  bash scripts/linux/install.sh --smoke
  bash scripts/linux/install.sh --run

The installer:
  1. installs build tools, Docker Engine and Docker Compose v2 when missing;
  2. installs the stable Rust toolchain for the invoking user when missing;
  3. starts Docker and grants the invoking user Docker access;
  4. downloads the pinned MySQL and PostgreSQL Docker images when missing.

No MySQL or PostgreSQL package is installed into the host operating system.
Database data is created later only under this project directory's .local-db/.

Options:
  --smoke  Install everything, then run the 100k/20k smoke benchmark.
  --run    Install everything, then run the full 30m/5m benchmark.
  --help   Show this help.

Run this script as your normal login user. It requests sudo only for operating
system packages, the Docker service and Docker group membership.
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

cleanup_temp() {
    if [[ -n "${TEMP_DIR}" && -d "${TEMP_DIR}" ]]; then
        rm -rf -- "${TEMP_DIR}"
    fi
}
trap cleanup_temp EXIT

case "${1:-}" in
    '')
        ;;
    --smoke)
        RUN_MODE=smoke
        ;;
    --run)
        RUN_MODE=full
        ;;
    --help|-h)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
(( $# <= 1 )) || die 'only one installer option may be specified'

[[ "$(uname -s)" == Linux ]] || die 'this installer supports Linux only'
[[ -r /etc/os-release ]] || die 'cannot detect Linux distribution'
[[ "$(getconf LONG_BIT 2>/dev/null || printf '0')" == 64 ]] ||
    die 'a 64-bit Linux system is required'

# shellcheck disable=SC1091
source /etc/os-release
readonly OS_ID="${ID:-}"
readonly OS_LIKE=" ${ID_LIKE:-} "
[[ -n "${OS_ID}" ]] || die '/etc/os-release does not define ID'

if (( EUID == 0 )); then
    ROOT_COMMAND=()
else
    command -v sudo >/dev/null 2>&1 ||
        die 'sudo is required; install it or run this script as root'
    sudo -v
    ROOT_COMMAND=(sudo)
fi

as_root() {
    if (( ${#ROOT_COMMAND[@]} == 0 )); then
        "$@"
    else
        "${ROOT_COMMAND[@]}" "$@"
    fi
}

if (( EUID == 0 )) && [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != root ]]; then
    TARGET_USER="${SUDO_USER}"
else
    TARGET_USER="$(id -un)"
fi
readonly TARGET_USER

TARGET_HOME="$(
    getent passwd "${TARGET_USER}" 2>/dev/null | awk -F: 'NR == 1 { print $6 }'
)"
[[ -n "${TARGET_HOME}" && -d "${TARGET_HOME}" ]] ||
    die "cannot determine home directory for ${TARGET_USER}"
readonly TARGET_HOME
readonly TARGET_PATH="${TARGET_HOME}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

run_as_target() {
    if [[ "$(id -un)" == "${TARGET_USER}" ]]; then
        "$@"
    else
        as_root runuser -u "${TARGET_USER}" -- "$@"
    fi
}

PACKAGE_FAMILY=''
if [[ "${OS_ID}" == ubuntu || "${OS_ID}" == debian ||
    "${OS_LIKE}" == *" ubuntu "* || "${OS_LIKE}" == *" debian "* ]]; then
    PACKAGE_FAMILY=apt
elif [[ "${OS_ID}" == fedora || "${OS_ID}" == rhel ||
    "${OS_ID}" == centos || "${OS_ID}" == rocky ||
    "${OS_ID}" == almalinux || "${OS_LIKE}" == *" rhel "* ||
    "${OS_LIKE}" == *" fedora "* ]]; then
    PACKAGE_FAMILY=dnf
else
    die "unsupported Linux distribution: ${OS_ID}; use Ubuntu, Debian, Fedora, RHEL, CentOS, Rocky or AlmaLinux"
fi
readonly PACKAGE_FAMILY

printf 'Installing required host tools for %s...\n' "${PRETTY_NAME:-${OS_ID}}"
case "${PACKAGE_FAMILY}" in
    apt)
        as_root apt-get update
        as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y \
            build-essential ca-certificates coreutils curl findutils \
            gawk grep pkg-config util-linux
        ;;
    dnf)
        as_root dnf -y install \
            ca-certificates coreutils curl findutils gawk gcc gcc-c++ \
            grep make pkgconf-pkg-config util-linux
        ;;
esac

TEMP_DIR="$(mktemp -d)"
chmod 755 -- "${TEMP_DIR}"

install_docker_apt() {
    local repository_os suite architecture key_file sources_file
    if [[ "${OS_ID}" == ubuntu || "${OS_LIKE}" == *" ubuntu "* ]]; then
        repository_os=ubuntu
        suite="${UBUNTU_CODENAME:-${VERSION_CODENAME:-}}"
    else
        repository_os=debian
        suite="${VERSION_CODENAME:-}"
    fi
    [[ -n "${suite}" ]] ||
        die 'cannot determine the matching Docker apt repository codename'
    architecture="$(dpkg --print-architecture)"
    key_file="${TEMP_DIR}/docker.asc"
    sources_file="${TEMP_DIR}/docker.sources"

    curl --proto '=https' --tlsv1.2 -fsSL \
        "https://download.docker.com/linux/${repository_os}/gpg" \
        -o "${key_file}"
    cat >"${sources_file}" <<EOF
Types: deb
URIs: https://download.docker.com/linux/${repository_os}
Suites: ${suite}
Components: stable
Architectures: ${architecture}
Signed-By: /etc/apt/keyrings/docker.asc
EOF
    as_root install -m 0755 -d /etc/apt/keyrings
    as_root install -m 0644 "${key_file}" /etc/apt/keyrings/docker.asc
    as_root install -m 0644 \
        "${sources_file}" /etc/apt/sources.list.d/docker.sources
    as_root apt-get update
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        docker-ce docker-ce-cli containerd.io \
        docker-buildx-plugin docker-compose-plugin
}

install_docker_dnf() {
    local repository_os repository_url
    case "${OS_ID}" in
        fedora)
            repository_os=fedora
            ;;
        centos)
            repository_os=centos
            ;;
        *)
            repository_os=rhel
            ;;
    esac
    repository_url="https://download.docker.com/linux/${repository_os}/docker-ce.repo"

    as_root dnf -y install dnf-plugins-core
    if dnf config-manager addrepo --help >/dev/null 2>&1; then
        as_root dnf config-manager addrepo --from-repofile \
            "${repository_url}"
    else
        as_root dnf config-manager --add-repo "${repository_url}"
    fi
    as_root dnf -y install \
        docker-ce docker-ce-cli containerd.io \
        docker-buildx-plugin docker-compose-plugin
}

if ! command -v docker >/dev/null 2>&1 ||
    ! docker compose version >/dev/null 2>&1; then
    printf '%s\n' 'Installing Docker Engine and Docker Compose v2...'
    case "${PACKAGE_FAMILY}" in
        apt)
            install_docker_apt
            ;;
        dnf)
            install_docker_dnf
            ;;
    esac
else
    printf '%s\n' 'Docker Engine and Docker Compose v2 are already installed.'
fi

command -v docker >/dev/null 2>&1 ||
    die 'Docker installation completed without a docker command'
docker compose version >/dev/null 2>&1 ||
    die 'Docker Compose v2 is unavailable after installation'

if ! docker info >/dev/null 2>&1 && ! as_root docker info >/dev/null 2>&1; then
    command -v systemctl >/dev/null 2>&1 ||
        die 'Docker is installed but systemctl is unavailable to start it'
    as_root systemctl enable --now docker
fi

docker_ready=false
for _ in {1..15}; do
    if docker info >/dev/null 2>&1 || as_root docker info >/dev/null 2>&1; then
        docker_ready=true
        break
    fi
    sleep 1
done
[[ "${docker_ready}" == true ]] ||
    die 'Docker daemon did not become ready'

GROUP_MEMBERSHIP_ADDED=false
if [[ "${TARGET_USER}" != root ]] &&
    ! id -nG "${TARGET_USER}" | tr ' ' '\n' | grep -Fxq docker; then
    as_root usermod -aG docker "${TARGET_USER}"
    GROUP_MEMBERSHIP_ADDED=true
    printf 'Granted Docker access to user %s.\n' "${TARGET_USER}"
fi

if ! run_as_target env HOME="${TARGET_HOME}" PATH="${TARGET_PATH}" \
    cargo --version >/dev/null 2>&1; then
    printf 'Installing the stable Rust toolchain for %s...\n' "${TARGET_USER}"
    rustup_installer="${TEMP_DIR}/rustup-init.sh"
    curl --proto '=https' --tlsv1.2 -fsSL \
        https://sh.rustup.rs -o "${rustup_installer}"
    chmod 644 -- "${rustup_installer}"
    run_as_target env \
        HOME="${TARGET_HOME}" \
        CARGO_HOME="${TARGET_HOME}/.cargo" \
        RUSTUP_HOME="${TARGET_HOME}/.rustup" \
        PATH="${TARGET_PATH}" \
        sh "${rustup_installer}" \
        -y --profile minimal --default-toolchain stable
else
    printf '%s\n' 'Rust and Cargo are already installed.'
fi

run_as_target env HOME="${TARGET_HOME}" PATH="${TARGET_PATH}" \
    cargo --version
run_as_target env HOME="${TARGET_HOME}" PATH="${TARGET_PATH}" \
    rustc --version

project_env_value() {
    local key="$1"
    [[ -f "${PROJECT_ROOT}/.env" ]] || return 0
    awk -F= -v wanted="${key}" '
        $1 == wanted {
            sub(/^[^=]*=/, "")
            sub(/\r$/, "")
            print
            exit
        }
    ' "${PROJECT_ROOT}/.env"
}

configured_mysql_image="$(project_env_value MYSQL_IMAGE)"
configured_postgres_image="$(project_env_value POSTGRES_IMAGE)"
MYSQL_IMAGE="${MYSQL_IMAGE:-${configured_mysql_image:-mysql:8.4.8}}"
POSTGRES_IMAGE="${POSTGRES_IMAGE:-${configured_postgres_image:-postgres:17.10}}"
readonly MYSQL_IMAGE POSTGRES_IMAGE

docker_host() {
    if docker info >/dev/null 2>&1; then
        docker "$@"
    else
        as_root docker "$@"
    fi
}

for image in "${MYSQL_IMAGE}" "${POSTGRES_IMAGE}"; do
    [[ -n "${image}" && "${image}" != -* &&
        "${image}" =~ ^[A-Za-z0-9._/:@-]+$ ]] ||
        die "invalid Docker image reference: ${image}"
    if docker_host image inspect "${image}" >/dev/null 2>&1; then
        printf 'Using cached database image: %s\n' "${image}"
    else
        printf 'Downloading database image: %s\n' "${image}"
        docker_host pull "${image}"
    fi
done

printf '%s\n' \
    'Installation complete.' \
    'MySQL/PostgreSQL were downloaded as Docker images; no host database package was installed.' \
    "Future database data will live only under ${PROJECT_ROOT}/.local-db/data."

case "${RUN_MODE}" in
    install)
        if [[ "${GROUP_MEMBERSHIP_ADDED}" == true ]]; then
            printf '%s\n' \
                'Open a new login shell (or run `newgrp docker`) before running:' \
                '  bash scripts/linux/run-one-click.sh --smoke'
        else
            printf '%s\n' \
                'Run the smoke test with:' \
                '  bash scripts/linux/run-one-click.sh --smoke'
        fi
        ;;
    smoke|full)
        benchmark_args=()
        if [[ "${RUN_MODE}" == smoke ]]; then
            benchmark_args+=(--smoke)
        fi
        printf 'Starting the %s benchmark...\n' "${RUN_MODE}"
        if docker info >/dev/null 2>&1 &&
            [[ "$(id -un)" == "${TARGET_USER}" ]]; then
            env HOME="${TARGET_HOME}" PATH="${TARGET_PATH}" \
                bash "${RUN_SCRIPT}" "${benchmark_args[@]}"
        else
            as_root runuser -u "${TARGET_USER}" -- env \
                HOME="${TARGET_HOME}" PATH="${TARGET_PATH}" \
                bash "${RUN_SCRIPT}" "${benchmark_args[@]}"
        fi
        ;;
esac
