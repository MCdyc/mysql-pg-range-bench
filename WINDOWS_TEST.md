# Windows 本机测试记录

> 历史结果说明：本页记录的两轮测试早于 `id` 主键变更，当时表没有主键，只有 `event_time` 索引。当前代码使用 `id` 主键和 `event_time` 普通索引，旧结果不能与新结构结果直接混用。

测试时间：2026-07-17（Asia/Shanghai）

这份记录用于确认双数据库环境、15 列数据生成、批量插入、时间索引、范围计数、Rust 计时和 JSON 输出链路能够在 Windows 上完整运行。它不是 3000 万行 Linux 正式性能结论。

## 环境

| 项目 | 实测值 |
|---|---|
| 操作系统 | Windows 11 专业版，10.0.26200（Build 26200） |
| CPU | AMD Ryzen 7 5800H，8 核 / 16 线程 |
| 内存 | 31.86 GiB |
| 数据所在磁盘 | C:，SAMSUNG MZVLB512HBJQ-000L2，NVMe，NTFS |
| 电源方案 | 平衡 |
| Rust | rustc / cargo 1.96.0，x86_64-pc-windows-msvc |
| MySQL | 8.4.9 Community Server，Windows ZIP 版 |
| PostgreSQL | 18.4，x86_64-windows |

MySQL 官方 ZIP 的本机 SHA-256：

```text
5795BA250E89290F7507ED3BCC6A655BE373616ABB58B877ACDEA71E1B8F4E8C
```

## 隔离实例

- MySQL 安装目录：`%LOCALAPPDATA%\Programs\MySQL\mysql-8.4.9-winx64`
- MySQL 数据目录：`%LOCALAPPDATA%\mysql-benchmark\data`
- MySQL 测试地址：`127.0.0.1:3306`，数据库/用户为 `benchmark`
- PostgreSQL 测试数据目录：`%LOCALAPPDATA%\postgres-benchmark\data`
- PostgreSQL 测试地址：`127.0.0.1:55432`，数据库/用户为 `benchmark`
- 原有系统 PostgreSQL 18.4 服务仍在 `5432`，没有修改它的配置、认证或数据。

两个隔离实例都只监听 `127.0.0.1`。PostgreSQL 测试实例使用 SCRAM-SHA-256；连接密码来自仓库根目录 `.env` 或程序默认测试配置。

启停命令已经实际验证过一次完整的“停止后再启动”：

```powershell
.\scripts\windows\start-local-databases.ps1
.\scripts\windows\stop-local-databases.ps1
.\scripts\windows\run-smoke.ps1
```

## 10 万行冒烟测试

参数：

```text
rows=100,000
scan_rows=20,000
batch_size=1,000
transaction_rows=10,000
warmups=2
runs=5
```

| 指标 | MySQL 8.4.9 | PostgreSQL 18.4 |
|---|---:|---:|
| 插入耗时 | 1,404.262 ms | 1,011.351 ms |
| 插入吞吐 | 71,212 rows/s | 98,878 rows/s |
| 实际 `COUNT(*)` | 20,000 | 20,000 |
| 热查询 median | 6.512 ms | 1.508 ms |
| 热查询 p95 | 6.999 ms | 1.784 ms |

两边生成指纹相同：`1cf3c2b7b421908e`。

完整结果：[windows-smoke-100k.json](benchmark-results/windows-smoke-100k.json)

## 100 万行验证测试

参数保持正式目标约六分之一的查询选择率：

```text
rows=1,000,000
scan_rows=166,667
range_rows=[416,666, 583,333)
batch_size=1,000
transaction_rows=100,000
warmups=2
runs=5
```

| 指标 | MySQL 8.4.9 | PostgreSQL 18.4 |
|---|---:|---:|
| schema setup | 53.74 ms | 17.55 ms |
| 插入耗时 | 11,948.375 ms | 10,845.125 ms |
| 插入吞吐 | 83,693 rows/s | 92,207 rows/s |
| 插入后维护 | 10.07 ms | 396.08 ms |
| 实际 `COUNT(*)` | 166,667 | 166,667 |
| 热查询 min | 51.082 ms | 9.778 ms |
| 热查询 median | 52.718 ms | 10.108 ms |
| 热查询 p95 / max | 64.621 ms | 10.986 ms |

五次原始查询耗时（ms）：

```text
MySQL:      [51.0819, 52.3402, 64.6206, 53.8605, 52.7184]
PostgreSQL: [9.8075, 10.6680, 10.9856, 10.1081, 9.7776]
```

两边生成指纹相同：`cfd873c7197cd15c`。数据库回查也确认：

- 实际总行数均为 1,000,000；
- 时间范围均为 `2024-01-01 00:00:00` 到 `2024-01-12 13:46:39`；
- 15 列全部 `NOT NULL`；
- 唯一用户显式索引是 `idx_benchmark_events_event_time(event_time)`；
- MySQL 计划为 `range`，`Using index`；
- PostgreSQL 计划为 `Index Only Scan`。

表和索引占用：

| 存储 | MySQL | PostgreSQL |
|---|---:|---:|
| 表/数据 | 191,578,112 B（182.70 MiB） | 186,179,584 B（177.55 MiB） |
| 索引 | 20,512,768 B（19.56 MiB） | 22,487,040 B（21.45 MiB） |
| 合计 | 202.27 MiB | 208,740,352 B（199.07 MiB） |

完整结果：[windows-smoke-1m.json](benchmark-results/windows-smoke-1m.json)

## 解读限制

- 这是 Windows、热缓存、单次装载结果；不能外推为 Linux 3000 万行结论。
- `--database both` 让两个服务同时常驻，并固定先测 MySQL、再测 PostgreSQL，存在顺序和后台 I/O 偏差。
- 数据库版本不同（MySQL 8.4.9、PostgreSQL 18.4），内部默认参数也不同。
- 插入计时包含 Rust 数据生成、FNV 指纹、进度输出、网络、索引维护和事务提交。
- PostgreSQL 的 `VACUUM (ANALYZE)` 与 MySQL 的 `ANALYZE TABLE` 分别计时，不属于查询耗时。
- 只有一次插入样本，不能据此排名；正式测试需在 Linux 上一次只运行一个数据库并采集多轮。
