#!/usr/bin/env bash
set -Eeuo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 is required (the 'docker compose' command)." >&2
  exit 1
fi

assume_yes=false
target=both
for arg in "$@"; do
  case "$arg" in
    --yes) assume_yes=true ;;
    mysql|postgres|both) target="$arg" ;;
    *)
      echo "Usage: $0 [--yes] [mysql|postgres|both]" >&2
      exit 2
      ;;
  esac
done

if [[ "$assume_yes" != true ]]; then
  echo "This deletes both benchmark data volumes, then starts: $target."
  read -r -p "Continue? [y/N] " answer
  [[ "$answer" == "y" || "$answer" == "Y" ]] || exit 0
fi

docker compose down --volumes --remove-orphans
bash scripts/db-up.sh "$target"
