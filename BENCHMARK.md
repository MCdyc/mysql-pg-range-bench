# MySQL 与 PostgreSQL 3000 万行基准口径

本文以仓库当前的 Rust 程序为准，说明它实际创建的数据、计时边界和结果含义，并给出正式对比实验的操作规范。程序不附带预设性能结论；所有耗时都必须在目标 Linux 主机上实际测量。

## 1. 默认测试定义

默认配置比较同一 Rust 客户端对 MySQL 和 PostgreSQL 执行以下工作：

1. 重建空表 `benchmark_events`，创建 `event_time` B-tree 索引。
2. 流式生成并插入 30,000,000 行确定性数据。
3. MySQL 执行 `ANALYZE TABLE`；PostgreSQL 执行 `VACUUM (ANALYZE)`。
4. 对居中的时间范围执行 `COUNT(*)`，结果必须精确等于 5,000,000。

默认是单客户端、单连接、无并发。连接池上限 `--pool-size` 默认为 1；增大该值也不会让当前插入或查询逻辑并行执行。程序为每个 MySQL 连接设置 `time_zone='+00:00'`，为每个 PostgreSQL 连接设置 UTC，并将 PostgreSQL 的 `max_parallel_workers_per_gather` 设为 0。

`--database both` 会先建立两个数据库连接池，然后按固定顺序完整运行 MySQL，再完整运行 PostgreSQL。每个选中的数据库只重建和插入一次。`--runs` 表示每个数据库的**计时查询次数**，不是插入轮数。若要获得 5 个独立插入样本，必须从外部启动 5 次程序并分别保存结果。

## 2. 表结构：15 列、全部非空

默认表名为 `benchmark_events`，可通过 `--table` 修改。表正好有以下 15 列，全部为 `NOT NULL`：

| # | 列名 | MySQL | PostgreSQL | 默认生成规则 |
|---:|---|---|---|---|
| 1 | `id` | `BIGINT` | `BIGINT` | 从 1 开始的连续行号 |
| 2 | `event_time` | `DATETIME(6)` | `TIMESTAMP(6) WITHOUT TIME ZONE` | 基准时间加零基行号秒 |
| 3 | `user_id` | `BIGINT` | `BIGINT` | 1..5,000,000 |
| 4 | `order_id` | `BIGINT` | `BIGINT` | 1,000,000,000..1,899,999,999 |
| 5 | `category_id` | `INT` | `INTEGER` | 1..1,000 |
| 6 | `status` | `INT` | `INTEGER` | 0..7 |
| 7 | `quantity` | `INT` | `INTEGER` | 1..20 |
| 8 | `score` | `INT` | `INTEGER` | 0..10,000 |
| 9 | `region` | `VARCHAR(16)` | `VARCHAR(16)` | 8 个固定地区名之一 |
| 10 | `device` | `VARCHAR(16)` | `VARCHAR(16)` | 6 个固定设备名之一 |
| 11 | `customer_name` | `VARCHAR(32)` | `VARCHAR(32)` | `user_` 加 16 位小写十六进制 |
| 12 | `email` | `VARCHAR(64)` | `VARCHAR(64)` | `u` 加 16 位小写十六进制和 `@example.test` |
| 13 | `city` | `VARCHAR(32)` | `VARCHAR(32)` | 12 个固定城市名之一 |
| 14 | `note` | `VARCHAR(64)` | `VARCHAR(64)` | `note-` 加 16 位小写十六进制 |
| 15 | `source` | `VARCHAR(16)` | `VARCHAR(16)` | 6 个固定来源名之一 |

MySQL 实际 DDL 等价于：

```sql
CREATE TABLE benchmark_events (
  id BIGINT NOT NULL,
  event_time DATETIME(6) NOT NULL,
  user_id BIGINT NOT NULL,
  order_id BIGINT NOT NULL,
  category_id INT NOT NULL,
  status INT NOT NULL,
  quantity INT NOT NULL,
  score INT NOT NULL,
  region VARCHAR(16) NOT NULL,
  device VARCHAR(16) NOT NULL,
  customer_name VARCHAR(32) NOT NULL,
  email VARCHAR(64) NOT NULL,
  city VARCHAR(32) NOT NULL,
  note VARCHAR(64) NOT NULL,
  source VARCHAR(16) NOT NULL
) ENGINE=InnoDB;

CREATE INDEX idx_benchmark_events_event_time
  ON benchmark_events (event_time);
```

PostgreSQL 实际 DDL 等价于：

```sql
CREATE TABLE benchmark_events (
  id BIGINT NOT NULL,
  event_time TIMESTAMP(6) WITHOUT TIME ZONE NOT NULL,
  user_id BIGINT NOT NULL,
  order_id BIGINT NOT NULL,
  category_id INTEGER NOT NULL,
  status INTEGER NOT NULL,
  quantity INTEGER NOT NULL,
  score INTEGER NOT NULL,
  region VARCHAR(16) NOT NULL,
  device VARCHAR(16) NOT NULL,
  customer_name VARCHAR(32) NOT NULL,
  email VARCHAR(64) NOT NULL,
  city VARCHAR(32) NOT NULL,
  note VARCHAR(64) NOT NULL,
  source VARCHAR(16) NOT NULL
);

CREATE INDEX idx_benchmark_events_event_time
  ON benchmark_events (event_time);
```

程序显式创建的索引只有 `event_time` 上的普通索引；`id` 不是主键，也没有唯一约束或索引。MySQL InnoDB 在没有主键和非空唯一键时可能使用内部隐藏聚簇标识，这是数据库物理实现差异，不是程序为 MySQL 添加的第二个用户索引。

每次非 `--skip-insert` 运行都会执行 `DROP TABLE IF EXISTS`。因此只能使用专用测试数据库，不能把程序指向含有同名业务表的库。

## 3. 确定性流式数据

默认种子是十进制 `20260715`，默认基准时间是 `2024-01-01T00:00:00Z`。对零基行号 `i`：

```text
id         = i + 1
event_time = 2024-01-01 00:00:00 + i 秒
```

因此 `event_time` 在插入顺序中严格单调递增，每一行恰好比上一行晚 1 秒，没有重复时间值。程序将 RFC 3339 的 `--base-time` 转成 UTC 后，以无时区时间值绑定到两个数据库；数据库连接的会话时区也由程序统一为 UTC。为保证 `DATETIME(6)` 与 `TIMESTAMP(6)` 存储完全相同，基准时间必须对齐到整微秒，且生成范围和查询上界必须留在 MySQL `DATETIME` 的 1000..9999 年范围内。

其他字段由种子和行号初始化的 SplitMix64 流生成。每行消费 8 个 `u64` 值 `r1..r8`，映射规则为：

```text
user_id     = 1 + r1 % 5,000,000
order_id    = 1,000,000,000 + r2 % 900,000,000
category_id = 1 + r3 % 1,000
status      = r4 % 8
quantity    = 1 + r5 % 20
score       = r6 % 10,001
region      = [north, south, east, west, central, northeast, northwest, coastal][r1 % 8]
device      = [android, ios, web, tablet, desktop, other][r2 % 6]
customer_name = "user_" + r7 的 16 位小写十六进制
email         = "u" + r8 的 16 位小写十六进制 + "@example.test"
city        = [beijing, shanghai, shenzhen, guangzhou, hangzhou, chengdu,
               wuhan, nanjing, xiamen, suzhou, tianjin, qingdao][r3 % 12]
note        = "note-" + (r4 XOR r7) 的 16 位小写十六进制
source      = [organic, ads, referral, direct, partner, campaign][r5 % 6]
```

程序在两个数据库各自的插入循环中，以相同种子和行号实时生成相同的逻辑行。生成过程、字符串格式化以及 `fnv1a64-length-prefixed-v1` 生成数据指纹计算都包含在各自的插入计时内。

JSON 中的 `generated_fingerprint` 是生成器输出序列的一致性标记，不是数据库内容的回读校验，也不是密码学哈希。选择 `--database both` 且执行插入时，程序会比较两次生成得到的指纹；此外还会校验每批受影响行数和范围查询的最终计数。报告中的示例行同样来自生成器，不代表从数据库抽样回读。

## 4. 精确命中 500 万行的范围

范围采用左闭右开语义：

```sql
SELECT COUNT(*)
FROM benchmark_events
WHERE event_time >= ? AND event_time < ?;
```

PostgreSQL 使用 `$1`、`$2` 占位符，MySQL 使用 `?`；两个边界都作为时间类型参数绑定，不拼接用户输入。

未指定 `--range-start-row` 时，程序按下面的公式把范围居中：

```text
range_start_row = (rows - scan_rows) / 2
range_end_row   = range_start_row + scan_rows
```

在默认 30,000,000 行和 `--scan-rows 5000000` 下：

```text
range_start_row = 12,500,000
range_end_row   = 17,500,000
lower           = 2024-05-24T16:13:20Z
upper           = 2024-07-21T13:06:40Z
```

因为每个零基行号对应唯一的秒值，区间 `[lower, upper)` 正好覆盖行号 `[12,500,000, 17,500,000)`，所以 `COUNT(*)` 必须返回 `5,000,000`。若返回其他值，程序立即报错。这里的 500 万是精确的匹配行数；优化器实际采用索引扫描、仅索引扫描还是其他计划，应以结果 JSON 中计时前采集的非 `ANALYZE` JSON `EXPLAIN` 为准。

## 5. 批次、事务和插入计时

默认参数为：

```text
batch_size       = 1,000 行/多值 INSERT
transaction_rows = 100,000 行/事务
```

即通常每个事务包含 100 个多值 `INSERT`；30,000,000 行共 30,000 个批次、300 个事务。15 列乘 1,000 行形成 15,000 个绑定参数。程序为两个数据库共同采用 60,000 个绑定参数的安全上限，因此 `--batch-size` 最大为 4,000。

插入使用 `std::time::Instant` 计时。计时从进入插入循环、首个事务开始之前启动，到最后一个事务提交成功并完成最终进度处理后结束。`insert.elapsed_ms` 和 `insert.rows_per_second` 包含：

- Rust 按批生成行和格式化字符串；
- 生成数据指纹计算；
- 构造多值 SQL、绑定参数和客户端处理；
- 网络往返、数据库写入、预建时间索引维护；
- 所有事务的 `BEGIN`/`COMMIT`。

连接建立、服务器版本读取、`DROP/CREATE TABLE`、创建索引以及插入后的统计信息维护不在 `insert.elapsed_ms` 中。删除旧表、建表和建索引合计记录为 `schema_setup_ms`；MySQL `ANALYZE TABLE` 或 PostgreSQL `VACUUM (ANALYZE)` 单独记录为 `analyze_ms`。

进度输出发生在插入计时区间内。JSON 会记录 `progress_every` 和 `includes_progress_logging=true`；对比不同轮次时必须保持相同输出频率，或统一使用 `--progress-every 0`。

程序没有失败重试逻辑；任何批次、提交、维护或查询失败都会终止本次运行，该结果不能当作有效轮次。

## 6. 查询计时和 JSON 结果

每个数据库在维护完成后，先在一个已获取的连接上执行不带实际运行信息的 JSON `EXPLAIN`，再在同一连接上执行预热和计时查询。默认值为：

```text
warmups = 2   # 记录到 warmup_ms，但不进入汇总
runs    = 5   # 记录到 measured_ms，并生成汇总
```

单次查询计时从 Rust 发起 `query_scalar` 前开始，到客户端取回并解码 `COUNT(*)` 标量后结束。连接池创建、连接获取、`EXPLAIN` 和维护不计入 `measured_ms`。每次预热和计时查询都会校验结果等于 `--scan-rows`。

`summary_ms` 根据 `measured_ms` 给出 `min`、`max`、`mean`、`median` 和最近秩定义的 `p95`。样本数只有默认的 5 时，p95 实际等于最大值；解释结果时应同时查看原始数组。

使用 `--database both` 时，一次程序运行的结构是：

```text
连接 MySQL 和 PostgreSQL
  -> MySQL：重建表 -> 插入一次 -> ANALYZE -> EXPLAIN -> 2 次预热 -> 5 次计时
  -> PostgreSQL：重建表 -> 插入一次 -> VACUUM ANALYZE -> EXPLAIN -> 2 次预热 -> 5 次计时
  -> 比较两次生成指纹 -> 写一个 JSON 文件
```

`--skip-insert` 会跳过删表、建表、建索引和插入，但仍会执行维护、`EXPLAIN`、预热和计时查询。使用它时，现有表必须具有本文的确切 15 列 schema、唯一的显式时间索引，并与本次 `--rows`、`--scan-rows`、`--range-start-row`、`--seed` 和 `--base-time` 相匹配。程序只会通过范围计数发现部分不匹配，不会回读校验全部字段或查询系统目录；因此 JSON 会把 `schema_status` 标为 expected/unverified，并把索引数量及“插入前已创建”状态写为 `null`。

## 7. Linux 运行示例

推荐使用项目本地实例入口，它会自动管理连接、清理测试数据并保留实例复用：

```bash
bash scripts/linux/run-one-click.sh --smoke
bash scripts/linux/run-one-click.sh
```

下面是自行提供数据库连接时的底层用法。构建 release 版本：

```bash
cargo build --release --locked
```

程序会自动读取当前目录的 `.env`。也可以在 shell 中覆盖连接地址后运行默认正式规模：

```bash
export MYSQL_URL='mysql://用户:URL编码密码@127.0.0.1:3306/benchmark'
export POSTGRES_URL='postgres://用户:URL编码密码@127.0.0.1:5432/benchmark'

./target/release/mysql-pg-range-bench \
  --database both \
  --rows 30000000 \
  --scan-rows 5000000 \
  --batch-size 1000 \
  --transaction-rows 100000 \
  --warmups 2 \
  --runs 5 \
  --seed 20260715 \
  --base-time 2024-01-01T00:00:00Z \
  --output benchmark-results/run-01.json
```

先用小规模数据检查连接和磁盘权限：

```bash
./target/release/mysql-pg-range-bench \
  --database both \
  --rows 100000 \
  --scan-rows 20000 \
  --output benchmark-results/smoke.json
```

实际参数和相应环境变量以 `--help` 为准。不传 `--output` 时默认写入 `benchmark-results/run.json`；连接 URL 不会写入 JSON。

## 8. 公平性规范

程序保证两边使用相同字段、相同逻辑行、相同批大小、相同事务行数、预建的单个时间索引以及同一种多值 `INSERT` 路径。它不会自动控制主机、缓存和数据库配置；正式结论还需要做到以下几点：

- 使用同一台专用 Linux 主机、同一类存储和文件系统，数据库数据目录分离；一个正式轮次只运行一个数据库服务。
- 分别用 `--database mysql` 和 `--database postgres` 运行，交换每轮先后顺序，例如 `MySQL, PostgreSQL, PostgreSQL, MySQL`，降低温度、后台刷盘和顺序偏差。
- 每个插入样本都从程序重建的空表开始。至少采集 5 次独立程序运行，报告全部原始插入值、中位数和 p95，不能只报告最快值。
- 固定 CPU 核集合、NUMA 节点、CPU governor、容器 CPU/内存/swap 限制，并记录 turbo、swap、温度和后台任务状态。
- 记录 Linux 发行版和内核、CPU、RAM、磁盘型号、控制器、文件系统、挂载参数、容器运行时、镜像摘要、Rust 版本及程序提交。
- 记录 MySQL/PostgreSQL 精确版本和实际生效配置，尤其是缓存、redo/WAL、检查点和持久化参数。程序已经固定禁用 PostgreSQL 单条查询的 gather 并行，不能在一边手工重新开启后仍与默认结果混比。
- 明确测试的是开箱配置还是同资源约束的调优配置；两组结果不能混合排名。
- 保持相当的持久化语义。若修改 MySQL `innodb_flush_log_at_trx_commit` 或 PostgreSQL `fsync`、`synchronous_commit`、`full_page_writes`，必须完整披露并单独成组。
- binlog、WAL 归档、逻辑复制、备份和监控会改变 I/O；应两边都关闭相应用途，或作为“含复制/归档成本”的单独实验。
- 插入期间不要同时运行另一数据库、备份、全盘扫描或其他负载。同步记录 CPU、RSS、磁盘利用率、I/O 字节和 redo/WAL 字节。
- 同时报告表与索引占用空间。InnoDB 与 PostgreSQL 的物理结构差异属于结果的一部分，不能用额外索引或不对称维护强行抹平。

当前程序在索引已存在时插入，并在查询前固定执行 MySQL `ANALYZE TABLE` 和 PostgreSQL `VACUUM (ANALYZE)`。PostgreSQL 的 `VACUUM` 有助于设置 visibility map，使只读装载表可能使用 index-only scan；维护耗时已经单列，但这仍是需要在报告中说明的数据库差异。若要研究“刚插完未维护”或“先插入后建索引”，应另建实验，不能把它与本程序默认结果混在一起。

## 9. 热缓存与冷缓存

缓存状态必须明确标注，不能把不同状态的查询放进同一个平均值。

### 热缓存

当前程序的内置查询序列适合报告为同一连接上的预热后结果：先执行默认 2 次预热，再连续计时默认 5 次。它不应被描述为独立的 5 次冷查询。若需要 10 个热缓存样本，可使用 `--warmups 2 --runs 10`，并报告 `measured_ms` 全部原始值以及汇总。

### 冷缓存

严格冷缓存需要在专用测试机上另行组织，不是当前程序自动完成的功能。基本流程是：

1. 在计时之外完成装载和统计信息维护，然后正常停止数据库。
2. 执行 Linux `sync`，由有权限的操作者清空 OS 页缓存，再启动目标数据库；同时确保另一数据库未运行。
3. 不做预热，只计第一条范围查询，并在每个冷样本前重复完整冷启动流程。
4. 至少采集 5 个独立样本，报告原始值、中位数和 p95。

只重启数据库不会清空 Linux 页缓存。清空页缓存是整机操作，只能在专用机器上进行；没有权限时应标记为“数据库重启、OS 缓存未知”，不能称为严格冷缓存。

还要注意，当前 `--skip-insert` 仍会在查询前执行 `ANALYZE TABLE` 或 `VACUUM (ANALYZE)`，可能重新预热数据或索引。因此它不能直接充当严格冷缓存的“只查询”入口。若要严格按上述流程用 Rust 计时，需要另行提供一个跳过维护的查询入口或独立查询程序，并把它明确列为不同于当前默认流程的实验工具。

## 10. 结果报告模板

```text
实验 ID / 日期：
程序 commit / Rust 版本：
主机：Linux/kernel=；CPU=；RAM=；磁盘/文件系统=；容器运行时=
资源约束：CPU 集合=；NUMA=；内存/swap=；governor/turbo=

数据：rows=30,000,000；seed=20260715；base_time=2024-01-01T00:00:00Z
表：benchmark_events；15 列全部 NOT NULL；显式索引数量=1；索引列=event_time
插入口径：batch=1,000；transaction_rows=100,000；索引预建=yes
范围行号：[12,500,000, 17,500,000)
范围时间：[2024-05-24T16:13:20Z, 2024-07-21T13:06:40Z)
期望/实际 count=5,000,000 / 

MySQL：精确版本=；关键配置=；镜像摘要=；表/索引大小=；EXPLAIN 摘要=
PostgreSQL：精确版本=；关键配置=；镜像摘要=；表/索引大小=；EXPLAIN 摘要=
持久化、binlog/WAL/归档口径：

独立插入轮次（每个值来自一次独立程序运行，毫秒 / rows/s）：
MySQL:      [ ] [ ] [ ] [ ] [ ]；median=；p95=
PostgreSQL: [ ] [ ] [ ] [ ] [ ]；median=；p95=
对应 generated_fingerprint：
schema_setup_ms：
维护耗时：MySQL ANALYZE TABLE=；PostgreSQL VACUUM ANALYZE=

热查询 measured_ms：MySQL=[ ]；PostgreSQL=[ ]
热查询汇总：各自 min/mean/median/p95/max=
冷查询（若另行严格执行）：MySQL=[ ]；PostgreSQL=[ ]；各自 median/p95=

原始 JSON 路径：
系统信息路径：
异常、后台活动、缓存状态和作废轮次：
```

## 11. 常见误读

- 把 `--runs 5` 说成程序自动插入 5 轮；它只执行 5 次计时查询。
- 把生成器说成在计时外准备好的数据源；当前实现是在每个数据库的插入计时内流式生成。
- 把 `event_time` 说成打散；当前实现严格按插入行号每行增加 1 秒。
- 把生成指纹当成数据库回读校验；它只是程序生成序列的 FNV-1a 64 位一致性标记。
- 把 `id` 说成主键，或漏报两个数据库都在插入前创建的时间索引。
- 把 `schema_setup_ms`、`analyze_ms` 算入插入耗时，或反过来忽略插入计时中包含的 Rust 生成、指纹和进度输出成本。
- 使用 `BETWEEN` 或闭区间导致多计右端点；当前查询固定使用 `[lower, upper)`。
- 只看“扫描约 500 万”而不检查 `observed_count`；当前默认匹配行数必须精确为 5,000,000。
- 把预热后的 5 个 `measured_ms` 当成 5 次冷启动结果。
- 一边使用 `COPY` 或专用 bulk loader，另一边使用当前多值 `INSERT`，却归因于数据库本身。
- 一边关闭 fsync/同步提交，另一边保持崩溃安全，或只让一边承担复制与归档日志成本。
- 只报告最快值，不提供原始 JSON、执行计划、硬件、版本、配置和缓存状态。
