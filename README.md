# MySQL / PostgreSQL 3000 万行范围查询基准

这是一套面向 Linux 的可复现基准：Rust 生成同一批确定性伪随机数据，分别写入 MySQL 与 PostgreSQL，再对带索引的时间列执行 `COUNT(*)` 范围查询。默认写入 30,000,000 行，查询范围精确覆盖 5,000,000 行。

> 本仓库提供测试程序和结果采集格式，不包含虚构的性能数字。性能结果必须在目标 Linux 主机上实际运行后生成。

## 测试模型

- 两库逻辑表结构相同，共 15 列，全部 `NOT NULL`。
- `event_time` 是唯一由本测试显式创建索引的字段；专用表名默认为 `benchmark_events`。
- `event_time` 从固定基准时间开始，每行递增 1 秒。
- 其他字段由固定种子和行号生成；相同行号在两个数据库中的字段值完全一致。
- 查询使用左闭右开范围：

```sql
SELECT COUNT(*)
FROM benchmark_events
WHERE event_time >= :lower_bound AND event_time < :upper_bound;
```

上面用命名符号表示绑定值；程序实际对 MySQL 使用 `?`，对 PostgreSQL 使用 `$1` / `$2`。

完整字段、数据分布和公平测试规范见 [BENCHMARK.md](BENCHMARK.md)。

## Linux 已有数据库一键隔离测试

如果 Linux 上已经运行 MySQL 和 PostgreSQL，可直接使用隔离入口。它为本次运行生成两个唯一临时数据库，测试结束、失败或收到常规终止信号后，按精确名称删除并验证不再存在；不会新增用户、修改配置、重启服务或接触已有业务库。

```bash
export MYSQL_ADMIN_URL='mysql://管理员:URL编码密码@127.0.0.1:3306/mysql'
export POSTGRES_ADMIN_URL='postgres://管理员:URL编码密码@127.0.0.1:5432/postgres'

# 先跑 10 万行 / 查询 2 万行
bash scripts/linux/run-one-click.sh --smoke

# 正式默认 3000 万行 / 查询 500 万行
bash scripts/linux/run-one-click.sh
```

基准 JSON 和清理回执会保留在 `benchmark-results/`。只有两个临时库均删除并经系统目录验证后，程序才以成功状态退出。完整权限要求、信号处理、异常恢复和“只能恢复逻辑对象，不能回滚 WAL/redo、日志与缓存”的边界见 [LINUX_ONE_CLICK.md](LINUX_ONE_CLICK.md)。

## Docker Linux 快速开始

前置条件：64 位 Linux、Rust stable、Docker Engine 与 Docker Compose v2。3000 万行会占用较多磁盘和数据库日志空间，正式运行前请确认有充足余量。

```bash
cp .env.example .env
bash scripts/db-up.sh both
cargo build --release --locked
```

同一个 Docker daemon 上若有多份本仓库副本，请为每份 `.env` 设置不同的 `COMPOSE_PROJECT_NAME`；重置脚本按该项目名删除两个测试卷。

程序会从当前目录自动读取 `.env`；模板中的 `MYSQL_URL` / `POSTGRES_URL` 已与 Compose 默认账户对应。若修改账户、密码或端口，也要同步修改这两个 URL（凭据中的 URL 特殊字符需百分号编码）。也可以用当前 shell 临时覆盖：

```bash
export MYSQL_URL='mysql://benchmark:benchmark_password@127.0.0.1:3306/benchmark'
export POSTGRES_URL='postgres://benchmark:benchmark_password@127.0.0.1:5432/benchmark'
```

先做小规模冒烟测试；具体参数以 `--help` 输出为准：

```bash
./target/release/mysql-pg-range-bench \
  --database both \
  --rows 100000 \
  --scan-rows 20000 \
  --output benchmark-results/smoke.json
```

确认成功后，可以用便捷双库模式完整跑一遍；它会让两个服务同时常驻，适合功能验证，不作为严格的正式性能排名：

```bash
./target/release/mysql-pg-range-bench \
  --database both \
  --rows 30000000 \
  --scan-rows 5000000 \
  --batch-size 1000 \
  --transaction-rows 100000 \
  --warmups 2 \
  --runs 5 \
  --output benchmark-results/run-01.json
```

程序会重建专用表 `benchmark_events`，随后依次记录建表、插入、统计信息维护、预热和多轮查询。插入时间是 Rust 客户端观察到的端到端时间，包含确定性流式生成、指纹计算、网络发送和事务提交；两库采用完全相同口径。不要把它指向含有同名业务表的数据库。

## 建议的正式测试方式

1. 固定 CPU、内存、磁盘、镜像版本和数据库配置，不要在测试期间运行其他重负载。
2. 先重置数据卷，再测试一种数据库；下一轮交换测试顺序，避免顺序偏差。
3. 插入吞吐与查询耗时分开比较；`VACUUM (ANALYZE)` / `ANALYZE TABLE` 也单独记录。
4. 热缓存结果至少运行 5 轮并报告中位数和 p95。冷缓存需要重启数据库并按 `BENCHMARK.md` 的流程单独测试。
5. 分享结果时同时提供生成的 JSON、`uname -a`、`lscpu`、`free -h`、磁盘型号/挂载方式和容器版本。

一次只运行一个数据库的正式轮次可直接使用默认规模：

```bash
bash scripts/db-reset.sh --yes mysql
./target/release/mysql-pg-range-bench \
  --database mysql --output benchmark-results/mysql-01.json

bash scripts/db-reset.sh --yes postgres
./target/release/mysql-pg-range-bench \
  --database postgres --output benchmark-results/postgres-01.json
```

第二次重置会删除两边的数据库卷，但不会删除宿主机上的 JSON 结果。

可在正式运行前后采集这些信息。脚本不输出数据库连接密码，也只采集本 Compose 项目的容器资源快照；但系统报告仍包含主机名、设备、挂载路径和源码文件名，外发前请按环境要求脱敏：

```bash
mkdir -p benchmark-results
bash scripts/system-info.sh > benchmark-results/system-info.txt
```

如需清空两个测试数据库并重建：

```bash
bash scripts/db-reset.sh
```

该脚本会先询问确认；`bash scripts/db-reset.sh --yes mysql` 会直接删除本项目的两个 Docker 数据卷，并且只启动 MySQL。正式测试建议一次只启动一个数据库：`bash scripts/db-up.sh mysql` 或 `bash scripts/db-up.sh postgres`，脚本会停止另一服务。

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
