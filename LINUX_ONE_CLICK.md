# Linux 项目本地可复用数据库

默认 Linux 入口不连接机器上已经安装或运行的 MySQL、PostgreSQL。它通过 Docker Compose 创建两套只属于当前项目目录的实例：

```text
.local-db/
├── credentials.env
├── data/
│   ├── mysql/
│   └── postgres/
└── instance.lock
```

`.local-db/` 已被 Git 忽略。数据库文件和随机生成的本地凭据会跨测试保留，因此第二次运行可以直接复用实例，不需要重新初始化。

## 空白主机自动安装

在已克隆的项目目录内，以普通登录用户执行（不要在命令前加 `sudo`，脚本会在安装系统组件时自行申请权限）：

```bash
bash scripts/linux/install.sh
```

该脚本支持 Ubuntu、Debian、Fedora、RHEL、CentOS、Rocky Linux 和 AlmaLinux，会按需安装编译工具、Docker Engine、Docker Compose v2、Rust stable，并自动下载固定版本的 `mysql:8.4.8` 与 `postgres:17.10` 镜像。已经存在的组件和镜像会直接复用。

安装并立即执行冒烟测试可合并为一条命令：

```bash
bash scripts/linux/install.sh --smoke
```

使用 `--run` 则会安装后立即执行正式 3000 万行测试。MySQL/PostgreSQL 不会作为整机软件包安装；Docker 镜像存放在 Docker 自身缓存中，实际数据库数据只写入项目的 `.local-db/data/`。

自动安装会给 Linux 增加 Docker、Rust 和编译工具，但不会安装或修改整机 MySQL/PostgreSQL。`delete-local-instances.sh` 只负责删除本项目数据库实例，不会卸载这些通用开发依赖。

## 已安装环境的前置条件

- 64 位 Linux；
- Rust stable 与 Cargo；
- Docker Engine；
- 支持 `docker compose up --wait` 的 Docker Compose v2；
- 当前用户能够访问 Docker daemon；
- 正式 3000 万行测试所需的充足磁盘空间。

默认只监听回环地址：

- MySQL：`127.0.0.1:13306`
- PostgreSQL：`127.0.0.1:15432`

需要改端口时使用 `LOCAL_MYSQL_PORT`、`LOCAL_POSTGRES_PORT`。默认入口拒绝管理员 URL 参数，并覆盖可能存在的数据库连接环境变量，因此不会误连整机数据库。

## 一键测试与复用

第一次先做冒烟测试：

```bash
bash scripts/linux/run-one-click.sh --smoke
```

正式规模：

```bash
bash scripts/linux/run-one-click.sh
```

每次执行固定遵循以下生命周期：

1. 对当前项目目录加文件锁，禁止同一实例并发测试或删除。
2. 检查固定版本的 MySQL、PostgreSQL 镜像；缺失时自动下载。
3. 启动或复用 `.local-db/data/` 中的 MySQL、PostgreSQL。
4. 测试前清理上一次不可捕获中断可能遗留的 `codex_range_bench_<UUID>` 数据库。
5. Rust 程序在两边创建本次专用随机数据库，写入相同数据并执行范围查询。
6. 正常、失败、`Ctrl+C`、`SIGTERM` 或 `SIGHUP` 后，Rust 按精确名称删除本次测试数据库。
7. 外层脚本再次只在这两套项目本地实例内枚举合法 UUID 名，删除遗留项并查询系统目录验证数量为零。
8. 保留 MySQL/PostgreSQL 实例、随机凭据和空的维护数据库，供下次测试复用。

测试结果与清理回执保留在 `benchmark-results/`，数据库表、索引和测试数据库不会保留。

## 完全删除本地实例

确认不再需要复用时执行：

```bash
bash scripts/linux/delete-local-instances.sh
```

无人值守方式：

```bash
bash scripts/linux/delete-local-instances.sh --yes
```

删除脚本会先取得同一项目锁，随后：

1. 停止并删除仅带有当前项目 Compose 标签的容器和网络；
2. 验证这些容器已经不存在；
3. 删除 `.local-db/data/mysql`、`.local-db/data/postgres`；
4. 删除 `.local-db/credentials.env`。

它不会按镜像名、端口或宽泛前缀删除其他 Docker 容器，也不会操作系统安装的 MySQL/PostgreSQL 数据目录。
Docker 镜像缓存会保留，可供以后重新创建实例；镜像本身不包含本次数据库数据。

## 其他入口

- 只启动或复用本地实例：`bash scripts/db-up.sh both`
- 完全删除后重新创建：`bash scripts/db-reset.sh`
- 明确连接已有整机数据库的旧模式：`bash scripts/linux/run-existing-one-click.sh`

已有整机数据库模式的权限与清理边界见 [LINUX_EXISTING_DATABASES.md](LINUX_EXISTING_DATABASES.md)。

## 状态边界

每次测试结束会恢复**项目本地实例中的逻辑测试数据状态**：本工具的随机数据库、表和索引为零，但实例本身继续运行和复用。

数据库日志、WAL/redo、缓存、性能计数以及数据目录曾经增长的历史不会因删除测试数据库而回滚。只有执行 `delete-local-instances.sh` 才会停止实例并移除完整数据目录。`SIGKILL`、断电或 Docker daemon 永久失联无法在当次自动清理；下一次正常运行会先清理合法前缀的遗留测试数据库。
