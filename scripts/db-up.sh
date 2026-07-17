#!/usr/bin/env bash
set -Eeuo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 is required (the 'docker compose' command)." >&2
  exit 1
fi

if ! docker compose up --help 2>&1 | grep -q -- '--wait-timeout'; then
  echo "This script requires a Docker Compose v2 release with 'up --wait-timeout' support." >&2
  exit 1
fi

target="${1:-both}"
case "$target" in
  mysql)
    services=(mysql)
    docker compose stop postgres >/dev/null 2>&1 || true
    ;;
  postgres)
    services=(postgres)
    docker compose stop mysql >/dev/null 2>&1 || true
    ;;
  both)
    services=(mysql postgres)
    ;;
  *)
    echo "Usage: $0 [mysql|postgres|both]" >&2
    exit 2
    ;;
esac

dotenv_value() {
  local key="$1"
  [[ -f .env ]] || return 0
  awk -F= -v wanted="$key" '
    $1 == wanted {
      sub(/^[^=]*=/, "")
      sub(/\r$/, "")
      print
      exit
    }
  ' .env
}

if [[ ! -f .env ]]; then
  echo "No .env file found; using the safe localhost defaults from docker-compose.yml."
  echo "For explicit settings, run: cp .env.example .env"
fi

wait_timeout="${COMPOSE_WAIT_TIMEOUT:-$(dotenv_value COMPOSE_WAIT_TIMEOUT)}"
wait_timeout="${wait_timeout:-300}"
recommended_free_gib="${BENCH_RECOMMENDED_FREE_GIB:-$(dotenv_value BENCH_RECOMMENDED_FREE_GIB)}"
recommended_free_gib="${recommended_free_gib:-50}"
if [[ ! "$wait_timeout" =~ ^[1-9][0-9]*$ ]]; then
  echo "COMPOSE_WAIT_TIMEOUT must be a positive integer (seconds)." >&2
  exit 2
fi
if [[ ! "$recommended_free_gib" =~ ^[0-9]+$ ]]; then
  echo "BENCH_RECOMMENDED_FREE_GIB must be a non-negative integer." >&2
  exit 2
fi

docker_root="$(docker info --format '{{.DockerRootDir}}' 2>/dev/null || true)"
if [[ -n "$docker_root" && -d "$docker_root" ]]; then
  available_bytes="$(df -PB1 "$docker_root" | awk 'NR == 2 { print $4 }')"
  recommended_bytes=$(( recommended_free_gib * 1024 * 1024 * 1024 ))
  if [[ "$available_bytes" =~ ^[0-9]+$ ]] && (( available_bytes < recommended_bytes )); then
    echo "Warning: only $(( available_bytes / 1024 / 1024 / 1024 )) GiB is free at DockerRootDir ($docker_root)." >&2
    echo "A 30-million-row run can need substantial table, index, WAL/redo, and temporary space." >&2
  fi
fi

docker compose up --detach --wait \
  --wait-timeout "$wait_timeout" "${services[@]}"
docker compose ps
