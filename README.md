# MySQL / PostgreSQL 3000 万行范围查询基准

这是一套面向 Linux 的可复现基准：Rust 生成同一批确定性伪随机数据，分别写入 MySQL 与 PostgreSQL，再对带索引的时间列执行 `COUNT(*)` 范围查询。默认写入 30,000,000 行，查询范围精确覆盖 5,000,000 行；随后使用两个并发事务验证 `FOR UPDATE SKIP LOCKED`，在 500 行时间范围内锁住前 100 行并要求另一连接精确返回其余 400 行。

> 本仓库提供测试程序和结果采集格式，不包含虚构的性能数字。性能结果必须在目标 Linux 主机上实际运行后生成。

## 测试模型

- 两库逻辑表结构相同，共 15 列，全部 `NOT NULL`。
- `id` 是主键，由 Rust 从 1 开始连续生成并显式写入；不使用数据库自增。
- `event_time` 另建普通 B-tree 索引；专用表名默认为 `benchmark_events`。
- `event_time` 从固定基准时间开始，每行递增 1 秒。
- 其他字段由固定种子和行号生成；相同行号在两个数据库中的字段值完全一致。
- 查询使用左闭右开范围：

```sql
SELECT COUNT(*)
FROM benchmark_events
WHERE event_time >= :lower_bound AND event_time < :upper_bound;
```

上面用命名符号表示绑定值；程序实际对 MySQL 使用 `?`，对 PostgreSQL 使用 `$1` / `$2`。

普通范围查询计时完成后，程序还会执行独立的行锁测试：

```sql
SELECT id
FROM benchmark_events
WHERE event_time >= :lower_bound AND event_time < :skip_locked_upper_bound
ORDER BY event_time
FOR UPDATE SKIP LOCKED;
```

该测试不使用聚合：连接 A 在 `READ COMMITTED` 事务中锁住候选范围前 100 行，连接 B 对全部 500 行执行上述查询。程序校验候选数为 500、持锁数为 100、返回数为 400、返回 ID 与持锁 ID 不相交，并验证 JSON `EXPLAIN` 实际选择了 `event_time` 索引；两个事务最后均回滚。

完整字段、数据分布和公平测试规范见 [BENCHMARK.md](BENCHMARK.md)。

## Linux 项目本地一键测试

空白 Linux 主机先执行自动安装（支持 Ubuntu、Debian、Fedora、RHEL、CentOS、Rocky Linux 和 AlmaLinux）：

```bash
bash scripts/linux/install.sh
```

它按需安装 Docker Engine、Docker Compose v2、Rust stable 和编译工具，并下载固定版本的 MySQL/PostgreSQL 镜像；不会向整机安装 MySQL/PostgreSQL 软件包。安装并立即进行冒烟测试可执行 `bash scripts/linux/install.sh --smoke`。

默认测试入口不会读取整机数据库连接，而是在项目目录下启动或复用两套隔离实例：

```text
.local-db/data/mysql
.local-db/data/postgres
```

先做 10 万行 / 查询 2 万行的冒烟测试：

```bash
bash scripts/linux/run-one-click.sh --smoke
```

正式默认 3000 万行 / 查询 500 万行，并执行 500/100/400 的 `SKIP LOCKED` 校验：

```bash
bash scripts/linux/run-one-click.sh
```

每次测试完成或发生可捕获失败后，只删除本工具的随机测试数据库、表和索引，并验证数量为零；MySQL/PostgreSQL 实例和随机本地凭据继续保留，下次直接复用。JSON 结果与清理回执保留在 `benchmark-results/`。

完全结束、不再需要实例时才执行：

```bash
bash scripts/linux/delete-local-instances.sh
```

该命令只停止当前项目的 Compose 容器，并删除 `.local-db/` 下的实例数据与凭据，不操作整机安装的数据库。完整生命周期、端口和异常恢复说明见 [LINUX_ONE_CLICK.md](LINUX_ONE_CLICK.md)。

如确实需要连接已有整机数据库，必须显式使用 `scripts/linux/run-existing-one-click.sh`；其边界见 [LINUX_EXISTING_DATABASES.md](LINUX_EXISTING_DATABASES.md)。

## Linux 一键插入后单次查询

下面的入口会使用项目本地实例插入完整数据，等待插入完成，不执行维护和预热，只执行一次范围 `COUNT(*)`，随后自动清理测试数据库并保留实例：

```bash
bash scripts/linux/run-query-once.sh
```

小规模验证：

```bash
bash scripts/linux/run-query-once.sh --smoke
```

## Linux 已有数据只读单次查询

如果数据库中已经保留了符合本项目 schema 和确定性时间规则的 `benchmark_events` 表，可用只读脚本测量一次范围查询：

```bash
MYSQL_URL='mysql://用户:URL编码密码@127.0.0.1:3306/benchmark' \
POSTGRES_URL='postgres://用户:URL编码密码@127.0.0.1:5432/benchmark' \
bash scripts/linux/run-existing-query-once.sh
```

该脚本固定执行零次预热和一次正式 `COUNT(*)`，并跳过插入、`ANALYZE/VACUUM` 和 `SKIP LOCKED`。它不会创建、修改或删除数据库对象，也不会管理数据库或 Linux 页缓存。严格冷缓存测试应先执行 `cargo build --release --locked --bin mysql-pg-range-bench`，再完成数据库重启与 OS 页缓存控制，最后使用 `run-existing-query-once.sh --no-build --database mysql`；测另一个数据库前必须重新重置缓存。

## 建议的正式测试方式

1. 固定 CPU、内存、磁盘、镜像版本和数据库配置，不要在测试期间运行其他重负载。
2. 先运行冒烟测试；正式运行时保持两套本地实例的资源限制一致。
3. 插入吞吐与查询耗时分开比较；`VACUUM (ANALYZE)` / `ANALYZE TABLE` 也单独记录。
4. 热缓存结果至少运行 5 轮并报告中位数和 p95。冷缓存需要按 `BENCHMARK.md` 的流程单独测试。
5. 分享结果时同时提供生成的 JSON、`uname -a`、`lscpu`、`free -h`、磁盘型号/挂载方式和容器版本。

需要从完全空白实例重新开始时：

```bash
bash scripts/linux/delete-local-instances.sh --yes
bash scripts/linux/run-one-click.sh --smoke
```

删除实例不会删除宿主机上的 JSON 结果。

可在正式运行前后采集这些信息。脚本不输出数据库连接密码，也只采集本 Compose 项目的容器资源快照；但系统报告仍包含主机名、设备、挂载路径和源码文件名，外发前请按环境要求脱敏：

```bash
mkdir -p benchmark-results
bash scripts/system-info.sh > benchmark-results/system-info.txt
```

如需完全删除两个项目本地实例并重建：

```bash
bash scripts/db-reset.sh
```

该脚本会先询问确认；`bash scripts/db-reset.sh --yes mysql` 会删除本项目目录下的两套实例数据，然后只重新启动 MySQL。`bash scripts/db-up.sh mysql|postgres|both` 可手动启动或复用指定实例。

## Windows 本机实例

当前 Windows 电脑已经部署了隔离的 MySQL 8.4.9 和 PostgreSQL 18.4 测试实例。它们不会注册系统服务；电脑重启后用下面的脚本启动：

```powershell
.\scripts\windows\start-local-databases.ps1
```

停止两个隔离实例：

```powershell
.\scripts\windows\stop-local-databases.ps1
```

重新运行默认 10 万行冒烟测试：

```powershell
.\scripts\windows\run-smoke.ps1
```

也可指定规模，例如 `.\scripts\windows\run-smoke.ps1 -Rows 1000000 -ScanRows 166667`。

MySQL 使用 `127.0.0.1:3306`，隔离 PostgreSQL 使用 `127.0.0.1:55432`；原有 PostgreSQL 5432 服务未修改。Windows 两轮实测的环境、原始耗时、执行计划与限制见 [WINDOWS_TEST.md](WINDOWS_TEST.md)。

## 本地验证

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
