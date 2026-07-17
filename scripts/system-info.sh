#!/usr/bin/env bash
set -Eeuo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

section() {
  printf '\n## %s\n' "$1"
}

section "captured_at_utc"
date --utc --iso-8601=seconds

section "kernel"
uname -a

section "os_release"
cat /etc/os-release

section "cpu"
lscpu

section "memory"
free -h
swapon --show

section "block_devices"
lsblk --output NAME,MODEL,SIZE,ROTA,TYPE,FSTYPE,MOUNTPOINTS

section "filesystems"
df -hT
findmnt --real --output TARGET,SOURCE,FSTYPE,OPTIONS

section "rust"
rustc --version --verbose
cargo --version --verbose
cargo tree --depth 1

section "benchmark_binary"
if [[ -x target/release/mysql-pg-range-bench ]]; then
  sha256sum target/release/mysql-pg-range-bench
fi

section "source_revision"
git rev-parse HEAD 2>/dev/null || true
git status --short

section "docker"
docker version
docker compose version
docker info --format 'DockerRootDir={{.DockerRootDir}} Driver={{.Driver}} CgroupVersion={{.CgroupVersion}} CgroupDriver={{.CgroupDriver}}'

section "compose_images"
docker compose images

image_list="$(docker compose config --images)"
mapfile -t image_names < <(printf '%s\n' "$image_list" | sed '/^$/d' | sort -u)
for image_name in "${image_names[@]}"; do
  if docker image inspect "$image_name" >/dev/null 2>&1; then
    docker image inspect --format \
      'Image={{join .RepoTags ","}} Id={{.Id}} Digests={{join .RepoDigests ","}} Created={{.Created}} Architecture={{.Architecture}}' \
      "$image_name"
  else
    printf 'Image=%s Status=not-present-locally\n' "$image_name"
  fi
done

section "container_limits"
container_list="$(docker compose ps --all --quiet)"
mapfile -t container_ids < <(printf '%s\n' "$container_list" | sed '/^$/d')
for container_id in "${container_ids[@]}"; do
  docker inspect --format \
    'Name={{.Name}} Image={{.Image}} NanoCpus={{.HostConfig.NanoCpus}} Memory={{.HostConfig.Memory}} MemorySwap={{.HostConfig.MemorySwap}} CpusetCpus={{.HostConfig.CpusetCpus}} Status={{.State.Status}} OOMKilled={{.State.OOMKilled}}' \
    "$container_id"
done

section "database_versions"
if docker compose ps --status running --services | grep -qx mysql; then
  docker compose exec --no-TTY mysql mysqld --version
fi
if docker compose ps --status running --services | grep -qx postgres; then
  docker compose exec --no-TTY postgres postgres --version
fi

section "container_snapshot"
if (( ${#container_ids[@]} > 0 )); then
  docker stats --no-stream "${container_ids[@]}" 2>/dev/null || true
else
  echo "No Compose project containers exist."
fi
