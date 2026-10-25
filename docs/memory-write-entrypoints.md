# 记忆写入入口决策摘要（Memory Write-Entrypoint Decisions）

> 本文档是代码注释的权威引用；完整分析报告见本地归档
> `docs/_internal/archive/review/memory-write-entrypoints-garbage-analysis.md`
> （gitignored，不入库）。

## 核心决策

> **低质量的数据还不如没有数据。** 只有确信有质量的数据才能写入长期记忆。
> 自动化、无价值闸门的写入通道一律不建；事件流（日志、工具失败、构建结果）
> 不进入长期记忆；所有写入必须回答「这条信息未来会被检索吗？是否具备复用价值？」；
> **死代码不保留**——无调用者的写入通道直接删除，避免日后被误用。

> **2026-10-23 定案（ADR-101 已实施）**：[ADR-101](adr/zh/ADR-101-memory-standalone-mcp-service.md) 把记忆全部逻辑剥到
> 独立服务 `acowork-memory`（Gateway supervisor 托管，MCP + 面板 REST 双表面）。下表的**有效入口 A / D / E / F
> 全部归属服务**：A = MCP `memory_store` 工具、D = Runtime 首启动 `POST /admin/bootstrap`（决策 9 的薄集成，
> 无记忆逻辑）、E = 服务 REST `/memory/*`（经 Gateway 反代）、F = 服务内 consolidation 调度器。
> Runtime 不留任何写入通道；auto-inject（读取侧旁路）整体删除。本档的管道语义不变。
>
> **一次性入口（不属于常态写入面）**：Node 迁移 pass `POST /admin/import`（ADR-101 决策 10）把服务化之前的
> 遗留库整库导入实例命名空间——原始 SQLite 字节体，服务端拒绝与非空命名空间合并（答 `skipped_existing`），
> 真实导入后 Node 删除本地记忆行。它是数据交接，不是新增的写入生产者。

## 已废弃通道（本变更删除 + 2026-09 ADR-068 revision 追加）

| # | 通道 | 写入目标 | 废弃原因 |
|---|------|---------|---------|
| B | `record_tool_failures` / `record_procedural_from_failure` | Procedural（fail_count） | 工具失败是运行时异常/待修 bug，非长期记忆；首行 80 字截断无复用价值 |
| B2 | `run_self_evaluation` | Autobiographical Limitation（批量） | 统计口径残缺（success_count 恒 0）→ 系统性假阳性；能力边界非工具计数器可推导 |
| G | `record_turn` / `ConversationRecord`（死代码） | Episodic | 无调用者；留档接口有被误用风险 |
| H | SQLite 记忆层 `auto_generate_limitation_nodes` | Autobiographical Limitation（批量） | 与 B2 同一逻辑的重复实现 |
| I（2026-09） | `generalization.rs::detect_simple_patterns` / `run_generalization` | ProceduralNode | 伪规则归纳（字符串全等计数 + action/tool_calls 文本特征 hack）；LLM 端已改由 `EpisodicDistiller::promote_procedures` 承担 |
| J（2026-09） | `MemoryManager::run_post_compaction_tasks` / `run_relationship_generation` / `run_history_compression` | Autobiographical Relationship / History | Relationship 30 天规则第二生产者违反 single-producer 原则且不经 opt-in；History 自动合并下线（回归测试 `post_compaction_tasks_do_not_write_relationship_nodes` 锁定） |
| C（2026-09-10） | 会话/压缩蒸馏 `EpisodeDistiller::write_summary_to_provider` | Episodic（摘要即蒸馏） | compaction 与 session 关闭尾蒸馏不再写记忆（方法已删，仅注释残留：`episode_distill.rs` 模块头、`loop_context.rs`、`loop_session.rs`）——记忆持久化下放给 Agent 自身（`prompts/system.md` 指导 LLM 主动 `memory_store`） |

## 有效写入入口（ADR-068 2026-09 revision + 2026-10 校对）

| # | 入口 | 写入目标 | 触发方式 |
|---|------|---------|---------|
| A | `memory_store` 工具 | **Episodic**（Episode + knowledge_subtype，不直写沉淀层） | LLM 主动调用 |
| D | manifest 引导（bootstrap） | Autobiographical Identity/Capability（source="manifest"，权威导入） | 启动时 |
| E | HTTP 管理 API | 任意节点 | 外部客户端 |
| F | `EpisodicDistiller`（consolidation 后台流水线，opt-in） | Knowledge / Procedural / Autobiographical（含 promotion_metadata 审计） | 后台周期任务，`[memory.distiller].enabled` 门控 |

> 入口 C 已上表“已废弃通道”（原“会话蒸馏”一行属 2026-09 之前的遗留描述，与代码不符）。

## 影响面

> 下列路径是 2026-09 变更时的快照；ADR-101 之后，`manager` / `consolidation` / `judge_llm` / `metrics`
> 已迁入 `core/acowork-memory/src/`（服务面在 `src/service/`，存储在 `src/sqlite/`），
> `core/acowork-runtime/src/agent/loop_memory.rs` 与 Runtime 侧记忆入口整体删除。

- `core/acowork-memory/src/manager.rs`：删除 `record` / `record_turn` /
  `record_tool_failures` / `record_procedural_from_failure` / `run_self_evaluation`，
  `ConversationRecord` 类型移出公共 re-export。
- `core/acowork-runtime/src/agent/loop_memory.rs`：删除 `record_tool_failures_to_memory`；
  `retrieve_and_inject_memories` 不再返回 node IDs（原消费者已删）。
- `core/acowork-runtime/src/agent/loop_.rs`：删除工具失败记录调用与
  `retrieved_memory_ids` 传递链；`execute_single_iteration` 移除未使用参数。
- `core/acowork-sqlite/src/consolidation/offline.rs`：删除
  `auto_generate_limitation_nodes`（步骤 7），步骤重编号。
- `core/acowork-runtime/src/agent/session_state.rs`：删除只增不读的 `turn_counter`。

