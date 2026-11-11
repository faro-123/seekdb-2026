# MiniVectorDB 旧测试适配包

这是从旧 seekdb/AMD 的向量测试与实验流程中重新设计的本地工具。它包含 3 个额外用例、独立的精确向量真值及 Recall 校验、可重复的小型数据生成，以及本地实验记录器。它不包含数据库实现、正式隐藏用例或官方基线，不计算比赛性能得分。

本包使用 Python 3.9+ 标准库。仓库基线已核对至官方 2026-competition-round2 的 f4d4e26653af823a7601668802f80e374ad6643c，通信协议为仓库根目录 STDIO_PROTOCOL.md 所述 JSONL。

## 额外 SQL 用例

manifest.json 与 cases/ 不修改官方 mysqltest/manifest.json。三个本地用例分别覆盖：

- legacy_filtered_topk：过滤先于 TopK、距离并列按主键、空结果、K 大于符合条件的数量。
- legacy_index_rollback_restart：建索引后的修改在事务内可见，回滚恢复，提交后重启及 ann 模式结果一致。
- legacy_failed_index_tx：多行插入含错误维度时整条语句失败，事务中之前的更新仍可提交，重启和索引查询保持一致。

在 Linux 上、从仓库根目录，已有可信本地数据库可执行程序时，可通过官方驱动的自定义包入口运行：

~~~sh
python3 protocol_check/run.py \
  --command '["/绝对路径/到数据库程序","--data-dir","{data_dir}"]' \
  --package tools/minivectordb-legacy-testkit \
  --output protocol-results/local-extra-001
~~~

命令及其参数按实际程序调整；输出目录必须不存在。官方驱动负责 JSONL ready、execute、shutdown、重启和模式切换；每个用例使用独立数据目录，重启时复用该用例的目录。当前官方传输实现使用 POSIX 进程和管道接口，Windows 主机直接运行可能不兼容，建议使用 Linux 或 WSL。提交前还需依官方工具说明测试最终 Docker 镜像。

本地测试中的 SyntheticSession 只核对官方驱动如何解析本包、分派重启和切换模式；它不实现 SQL，不代表这些用例在数据库上通过。

## 生成精确真值和检查结果

从本目录执行，以下输出文件都必须是新文件：

~~~sh
python -m minivectordb_testkit generate --out local-data.json \
  --rows 100 --dim 4 --queries 10 --k 10 --seed 20261110
python -m minivectordb_testkit truth --dataset local-data.json --out reference.json
python -m minivectordb_testkit check --dataset local-data.json \
  --results candidate-results.json --mode ann --out report.json
~~~

数据集的 rows 是含 id、category、vector 的对象数组；queries 是含 id、vector、k、category 的对象数组，其中 category 为 null 表示无过滤。生成器交替构造无过滤和等值过滤查询，并记录随机种子。

candidate-results.json 是以查询 id 为键的对象，每个值是按查询顺序排列的结果数组，每个结果形如 {"id": 1, "distance": 0.0}。它是本工具的离线交换格式，**不是数据库的 JSONL 通信协议**。应从实际数据库结果生成此文件；reference.json 只用于观察或校验本地真值，不能冒充数据库输出。

真值先将向量元素转换为有限 float32，再过滤、计算欧氏距离并按距离与主键排序。检查器验证数量、ID 唯一性及有效性、过滤条件、结果顺序和距离；以 min(K,N) 为分母，空集不纳入平均，有过滤和无过滤分别统计。exact 模式还要求 ID 与精确参考一致；ann 模式默认按两组均值 0.95 作本地门槛检查。一组无有效查询时报告 incomplete，而不是通过。check 退出码：pass 为 0、fail 为 1、incomplete 或输入错误为 2。

工具以已舍入 float32 值的精确平方距离排序，输出距离采用双精度近似；距离比较容差为绝对及相对 1e-5。官方隐藏参考的中间计算精度尚未明确，特别接近的距离可能出现不同排序，正式数据分布和资源配置也以冻结题包为准。默认数据集很小，仅用于正确性检查；精确有理数计算在正式性能规模下可能非常慢。本工具的本地门槛不能当作平台分数或准入证明。

## 实验记录

运行可信命令，并把命令、输入文件 SHA-256、版本标签、墙钟耗时、退出状态及 stdout/stderr 放入新目录：

~~~sh
python -m minivectordb_testkit.capture --out local-run-001 \
  --input local-data.json --revision your-source-revision -- \
  python -c 'import os; print(os.environ["MINIVECTORDB_TEST_DATA_DIR"])'
~~~

记录器使用独立的 work 和 data 目录，不主动调用 shell，不连接或停止现有数据库。Windows 下拒绝直接或通过 PATH 解析到 .bat/.cmd 的命令，以免系统隐式调用命令解释器。命令仍可自行访问其拥有权限的其他路径，甚至自行启动 shell，因此只运行可信命令。输入文件运行前后哈希变化时状态为 input_changed，而非成功结果；现有输出目录不会被覆盖。记录的退出状态与时间仅针对直接启动的进程；无论它正常退出还是超时，都不保证其派生进程已经停止，后台进程也可能继续写入。记录的命令和输出可能包含敏感信息，提供参数和分享记录时应自行检查。

墙钟时间仅是本地命令记录；本工具不采集 cgroup io.stat 或 memory.peak，也不复刻平台预热、持久化和计时边界，不能据此计算正式性能指标 E。

## 自测

~~~sh
python -m unittest discover -s tests -v
~~~

这些测试验证的是辅助工具和用例结构；没有执行 MiniVectorDB 实现，不代表数据库功能或正式验收通过。
