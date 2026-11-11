# MiniVectorDB

MiniVectorDB 是单进程、单客户端的 Rust 数据库。容器从 `/data` 恢复后通过 stdin/stdout 提供 [JSONL 协议](STDIO_PROTOCOL.md)。建表、增删改查、向量索引和事务使用同一份持久化状态。直接运行二进制时可用 `--data-dir PATH` 指定独立目录。

## 构建和启动

在本仓库根目录的 Linux 或 WSL Bash 中执行。示例先提交一行、回滚另一行，再用同一数据卷启动新进程，核对普通查询与向量检索结果。

```bash
image=minivectordb:local
docker build --platform linux/amd64 -t "$image" .
data_volume="minivectordb-demo-$(date -u +%Y%m%dT%H%M%S)-$$"
docker volume create "$data_volume"
run_db() {
  docker run --rm -i --network=none --read-only --cap-drop=ALL \
    --security-opt=no-new-privileges --cpus=2 --memory=2g --memory-swap=2g \
    --pids-limit=128 --mount "type=volume,src=$data_volume,dst=/data" "$image"
}

run_db <<'JSONL'
{"id":1,"op":"execute","sql":"CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(2));","search_mode":"exact"}
{"id":2,"op":"execute","sql":"BEGIN;","search_mode":"exact"}
{"id":3,"op":"execute","sql":"INSERT INTO docs VALUES (9007199254740993, '[3,4]');","search_mode":"exact"}
{"id":4,"op":"execute","sql":"COMMIT;","search_mode":"exact"}
{"id":5,"op":"execute","sql":"BEGIN;","search_mode":"exact"}
{"id":6,"op":"execute","sql":"INSERT INTO docs VALUES (2, '[0,0]');","search_mode":"exact"}
{"id":7,"op":"execute","sql":"ROLLBACK;","search_mode":"exact"}
{"id":8,"op":"shutdown"}
JSONL

run_db <<'JSONL'
{"id":9,"op":"execute","sql":"SELECT id FROM docs ORDER BY id;","search_mode":"exact"}
{"id":10,"op":"execute","sql":"SELECT id,L2_DISTANCE(embedding,'[0,0]') AS distance FROM docs ORDER BY distance LIMIT 1;","search_mode":"exact"}
{"id":11,"op":"shutdown"}
JSONL
```

重启后，普通查询返回 `[[9007199254740993]]`，向量查询返回 `[[9007199254740993,5.0]]`。也可使用 Rust 1.99.1 在本机运行 `cargo build --locked --release`。依赖由 `Cargo.lock` 锁定，数据库核心使用 Rust 标准库和自研实现，serde/serde_json 负责序列化。

镜像默认以 UID/GID `0:0` 启动，以兼容 root 拥有的私有 `/data` 绑定目录。复用旧镜像以 UID/GID `10001:10001` 创建的本地数据卷时，在 `docker run` 增加 `--user 10001:10001`。

## SQL 与错误响应

支持单表 `CREATE TABLE`、`CREATE VECTOR INDEX`、`INSERT`、`UPDATE`、`DELETE`、`SELECT` 及 `BEGIN`、`COMMIT`、`ROLLBACK`。列类型为 `BIGINT`、`VARCHAR(n)` 和 `VECTOR(n)`。查询支持投影和别名、`WHERE` 条件合取、`ORDER BY`、`LIMIT` 和 `L2_DISTANCE`。

SQL 或输入校验失败时，响应返回原请求 ID、`ok: false` 与 `error.code`、`error.message`。语句暂存用于保持失败语句的原子性。启动或持久化遇到不可继续的 I/O 错误时，进程退出并通过 stderr 报告。

## 存储和执行

- `src/protocol.rs` 在恢复后发送 ready，原样回传 JSON id，逐请求刷新。shutdown 放弃显式事务的暂存修改。
- `src/sql.rs`、`src/types.rs` 和 `src/engine.rs` 分别负责 SQL、类型和事务视图。BIGINT 直接解析为 i64，VARCHAR 按 Unicode 字符数校验，向量的十进制元素直接舍入为有限 float32。
- `src/storage.rs` 和 `src/disk_index.rs` 把行与目录变更追加到 `commits.mv1`。事务帧包含长度、校验值和完成标记。成功提交响应前同步日志并更新派生索引。每条写语句先在独立磁盘暂存区校验，通过后才并入事务。恢复时重放完整帧、截掉不完整尾部并重建索引。目录独占锁保护恢复和派生文件。
- `src/query.rs` 和 `src/distance.rs` 在可见记录上先过滤，再比较基于 float32 位模式计算的精确平方距离，按主键打破并列，投影时以 float64 输出欧氏距离。平面向量索引直接定位已提交的二进制向量，需要事务覆盖层或其他列时可从完整行视图读取。exact 和 ann 请求都执行精确搜索。
- 适用的只读 TopK 可复用有内存预算的已提交数据快照。硬件支持且数值可精确还原时使用紧凑向量，区间不确定时按原精确距离复核。事务、超预算及其他不适用查询走有界扫描路径。
- WAL 按向量选择可逐位还原的紧凑编码，其他值保留原始 float32。新版可恢复旧 WAL，旧版只能读取未写入紧凑向量的 WAL。
- 小 TopK 使用有预算的堆，大排序使用磁盘归并。排序保留键和行位置，归并路数随最宽键调整，投影时再读取命中行。可用 `MINIVECTORDB_SORT_BYTES` 调小排序预算以复现溢出。

日志保留全部已提交历史，恢复时间随日志增长。单条 JSONL 请求及 SQL 语法树驻留内存。外部归并同时保留每个输入流的一个排序键，超宽 VARCHAR 排序键仍需要相应的内存。大结果可用 LIMIT 分批查询，公开驱动的单响应限制为 16 MiB。

## 验证

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked

evidence="protocol-results/$(date -u +%Y%m%dT%H%M%S)-$$"
python3 protocol_check/run.py --image "$image" --handshake-only --output "$evidence/handshake"
python3 protocol_check/run.py --image "$image" --output "$evidence/public"
python3 protocol_check/run.py --image "$image" --package tools/minivectordb-legacy-testkit --output "$evidence/legacy"
```

`tools/minivectordb-legacy-testkit` 包含本地迁移 SQL 用例和独立真值工具。补充验证入口是 `tools/verify_database.py` 和 `tools/verify_extended.py`。后者提供 `oracle`、`crash`、`large` 和 `wide` 四种场景，每次指定新的 `--out` 和 `--data-root`。oracle 从实际 JSONL 响应提取结果并交给迁移包校验，`large` 和 `wide` 应在启用 cgroup v2 的 96 MiB 容器中运行。

```bash
docker run --rm --network=none --cpus=2 --memory=96m --memory-swap=96m \
  --mount "type=bind,src=$PWD,dst=/workspace" --mount type=volume,dst=/dbtest \
  --mount "type=image,src=$image,dst=/submission" --workdir /workspace \
  python:3.11-slim-bookworm python3 tools/verify_extended.py --scenario large \
  --binary /submission/minivectordb --out "$evidence/large" --data-root /dbtest/run
```

`tools/dev_capture.py` 可记录命令、输入指纹、退出码和原始 stdout/stderr。赛题学习材料位于 `agent/conversation.json`。
