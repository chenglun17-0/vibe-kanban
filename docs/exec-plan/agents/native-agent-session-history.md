# 三 Agent 原生会话历史与执行器收敛计划

- 状态：待核验
- 当前阶段：全部 6 个 Phase 已实施，待人工核验
- 最近更新：2026-08-12
- 关联变更：任务说明——历史恢复改用 PI、Codex、Claude Code 原生会话文件，并移除其他 Agent 运行时支持

## 概述

### 问题与事实

用户进入已完成任务时需要看到 Agent 已物化的最终会话，而不是重放生成会话的 token delta。当前 `stream_normalized_logs` 在内存 store 缺失时从 `execution_process_logs` 读取 raw stdout，再启动对应 executor normalizer；前端通过 `/normalized-logs/ws` 逐 process 重放 JSON Patch。这个设计把 live 传输协议误当成持久历史协议。

本地问题样本提供了可复现证据：

- PI 任务 `019ff384-3418-7eee-a88a-8de08b45d41c` 的 raw log 有 104,905 行、约 12.4 MB，其中 55,347 行是 `message_update`，44,792 行是空 stdout。
- 同一任务的 PI 原生会话文件只有 125 行、约 758 KB，已包含 1 条 user、45 条 assistant、76 条 tool result，以及完整 thinking/tool call block。
- raw-log 重放的端到端实验耗时 65.9 秒；broadcast 容量为 10,000，消费者滞后时 `Lagged` 被静默丢弃，最终只恢复出 3 条 entry。因此它既不满足性能，也不能作为可靠兜底。
- `coding_agent_turns.agent_session_id` 已记录 Agent 原生 session ID；Codex 已有 rollout 定位和 `RolloutRecorder::get_rollout_history`，PI 已通过 RPC 上报 session ID，Claude Code 已解析 session ID 并用 `--resume` 续聊。

### 第一性约束

- 已完成会话的事实源是 Agent 自己成功持久化的原生 session/rollout 文件；raw stdout 是诊断日志，不是历史模型。
- 产品只支持 PI、Codex、Claude Code。其余 executor 不继续维护运行、配置、文档和测试路径。
- Vibe 的 UI 只消费统一 `NormalizedEntry`，不能把三种私有格式暴露到前端。
- 同一个 Agent session 文件包含 initial request 和 follow-up；历史必须按 Vibe `session` 读取一次，不能按 execution process 重复解析同一原生文件。
- live 增量和持久历史必须有明确切换点：运行中允许临时 overlay，结束后只信原生文件，禁止将两种来源长期混合。

### 目标

1. 为 PI、Codex、Claude Code 提供 `NativeSessionHistoryProvider`，把各自原生文件确定性转换为统一 `NormalizedEntry`。
2. 已完成 coding-agent 历史从原生文件读取；setup/cleanup script 仍从 raw logs 读取并按时间合并。
3. 运行中使用“原生已落盘历史 + 当前进程临时 overlay”；收到完成状态后重新读取原生文件并替换 overlay。
4. 原生文件缺失、格式不支持或解析失败时显式显示“历史不可用”，保留 Raw Logs 诊断入口；禁止静默回退到 raw-log normalizer。
5. 移除 PI、Codex、Claude Code 之外的 Agent 运行时支持，不保留任何旧实现兼容层；含 legacy executor 的旧数据在读取时显式报错（见“已确认决策” 2026-08-12 更新）。
6. PI 历史读取使用 active branch 并遵守 compaction；Codex 使用官方 rollout 类型；Claude Code 复用现有 `ClaudeJson`/tool 映射语义。

### 非目标

- 不把三种原生文件复制进 Vibe 数据库；本期接受“文件与服务同机、由 Agent 管理生命周期”的产品约束。
- 不实现远程机器抓取、云端同步或跨设备历史；remote deployment 若无法访问原生文件，返回明确不可用状态。
- 不为已移除 Agent 保留执行或 raw-log 历史重建能力；旧数据只保留元信息和 Raw Logs 诊断能力。
- 不重新设计消息 UI、diff 视图或脚本日志模型；只替换 coding-agent 历史来源。
- 不承诺兼容任意未来私有格式；三类 provider 以 fixture 和受支持版本矩阵约束。

### 需求与验收场景

- **R1 原生历史正确性**：给定三种带 user/assistant/thinking/tool call/tool result 的 fixture，provider 输出稳定且不含 streaming delta。场景：同一文件重复解析两次，输出完全相同。
- **R2 分支与压缩语义**：PI 只展示 active branch，compaction 使用 `buildContextEntries()` 等价语义；Codex/Claude Code 排除非当前会话和隐藏控制记录。场景：fixture 包含旁支或 checkpoint，UI 不重复展示已被压缩/切走的历史。
- **R3 会话级去重**：同一 Agent session 被 initial/follow-up 多个 process 复用时只解析一次。场景：两次 follow-up 后历史中的 user 消息各出现一次。
- **R4 live 切换**：运行时 overlay 可见；进程完成后原生文件成为唯一历史，overlay 被整体替换。场景：最终 assistant message 不重复、不截断。
- **R5 明确失败**：文件缺失、权限不足、格式版本不支持、尾部半行时分别返回结构化错误。场景：页面停止 Loading，显示可操作错误及 Raw Logs 入口。
- **R6 执行器收敛**：新建任务和配置仅出现 PI/Codex/Claude Code。场景：代码与配置搜索不再命中已删除 Agent（历史 migration 除外）；含 legacy executor 的旧数据在反序列化时直接报错，不提供只读兼容 DTO。
- **R7 性能**：使用本问题 758 KB PI 会话 fixture，在 dev 构建下 provider 解析 P95 < 500 ms，接口首条响应 P95 < 1 s；重复读取命中基于文件 metadata 的缓存后 P95 < 100 ms。

## 设计

### 候选比较

| 候选 | 核心做法 | 改动面 | 新增依赖 | 主要失败模式 | 结论 |
| --- | --- | ---: | --- | --- | --- |
| A. 原生文件 provider（推荐） | 三个 provider 直接读取 Agent 已物化会话并映射为 `NormalizedEntry` | 中 | PI SDK 需确认链接方式；Codex/Claude 复用现有依赖与类型 | 文件缺失、私有格式升级、active branch 选错 | 唯一同时满足原生语义和读取性能的方案 |
| B. Vibe 数据库 projection | live 时另存统一 entry 表 | 大 | schema migration | 双写一致性、迁移、重放修复 | 可靠但用户明确不需要复制物化数据，拒绝额外事实源 |
| C. raw-log batch reducer | 每次读取 raw stdout 后批量归一化 | 中 | 无 | 每次 O(raw bytes)，仍维护 delta/bridge 私有协议 | 不满足“直接用 Agent 物化文件”，且问题样本输入膨胀 16 倍 |
| D. 维持现状 | WebSocket 重放 patch | 小 | 无 | O(L²)、broadcast 丢数据、无限 Loading | 已由端到端证据排除 |

选择 A。简单方案 D 已被正确性和性能证据排除；C 仍把传输事件误作持久事实；B 引入第二份持久事实，与本期产品约束冲突。

### 统一入口与数据边界

在 `crates/executors` 新增只读 provider 边界（路径为计划新增）：

```rust
pub trait NativeSessionHistoryProvider {
    fn locate(&self, session_id: &str, cwd: &Path) -> Result<PathBuf, NativeHistoryError>;
    fn fingerprint(&self, path: &Path) -> Result<FileFingerprint, NativeHistoryError>;
    fn read(&self, path: &Path) -> Result<NativeSessionHistory, NativeHistoryError>;
}

pub struct NativeSessionHistory {
    pub entries: Vec<NormalizedEntry>,
    pub source: NativeHistorySource,
    pub fingerprint: FileFingerprint,
}
```

核心实现放在 `crates/executors/src/history/`（新增），具体 provider 放在现有 executor 子模块，避免通用层依赖私有格式。服务层新增 session 级历史入口，例如 `/api/sessions/{session_id}/conversation-history/ws`（新增）；前端不再逐 coding-agent process 请求相同原生 session。

`FileFingerprint` 至少包含 canonical path、size、modified time。缓存键为 `(agent, native_session_id, fingerprint)`；缓存只保存转换后的 entries，不缓存失败超过短 TTL。文件变化时自然失效，不使用定时刷新。

### 三种 provider

#### PI

- 位置：`~/.pi/agent/sessions/--<cwd>--/<timestamp>_<session-id>.jsonl`；允许用 session ID 扫描，以原生 header 的 `id` 二次校验，不能只信文件名。
- 语义：优先复用 PI `SessionManager.open()`、`getBranch()`/`buildContextEntries()`；禁止手写“按 JSONL 行顺序展示”，因为文件是 `id`/`parentId` 树且含 compaction、branch summary、custom message。
- 映射：user、assistant text/thinking/toolCall、toolResult、bashExecution、可显示 custom message、compaction/branch summary；隐藏 model/settings/label/custom state 等非对话记录。
- 安全：拒绝 session header ID 不匹配和不在 PI session root 下的路径；图片只返回受控引用，不把 base64 原样塞入 entry 文本。

#### Codex

- 位置：复用 `SessionHandler::find_rollout_file_path`，不新增第二套目录扫描。
- 解析：复用现有 `RolloutRecorder::get_rollout_history` 和 `RolloutItem`；从已物化的 `response_item` 生成 assistant/reasoning/function call/output，从 `event_msg` 提取必要的最终 agent message 和 token usage，忽略 delta/进度事件。
- 去重：同一逻辑内容若同时存在 `response_item::message` 与 `event_msg::agent_message`，以 rollout item 的稳定 ID/顺序规则去重，规则由 fixture 锁定。

#### Claude Code

- 位置：按 session ID 在 Claude projects root 下定位 `{session-id}.jsonl`，读取 header/sessionId 校验；cwd 编码目录只作为候选缩小，不作为唯一真源。
- 解析：复用 `ClaudeJson`、`ClaudeLogProcessor::normalize_entries` 的 tool/diff 映射，但增加 batch/file reader，跳过 `stream_event`、queue/control、replay 和隐藏 sidechain；以最终 assistant/user/tool records 为准。
- 分支：按 `uuid`/`parentUuid` 选择当前主链；若原生文件缺少可靠 leaf 标记，使用最后一个非 sidechain、非 meta entry 作为 leaf，并把该规则写入 fixture 测试。不能简单按文件行顺序合并所有 sidechain。

### 历史合并与 live 行为

1. 服务按 Vibe `session_id` 查询最新 `coding_agent_turns.agent_session_id` 和固定的 `sessions.executor`。
2. provider 读取一次原生会话，生成 coding-agent entries。
3. setup/cleanup script 继续读取 raw logs，生成 script tool entries；按 process `created_at` 插入对应 user turn 前后。
4. 运行中 process 保留现有 normalized WS 作为 **overlay only**，但只覆盖 native 文件尚未物化的当前 turn；overlay 带 `execution_process_id`，不能写入 native entries。
5. process 完成或失败后，服务发 `history_invalidated`，前端丢弃整个 overlay 并重新读取原生文件。若原生文件在限定时间内未出现最终记录，显示“Agent 尚未完成持久化”，短暂重试，不回退 raw normalization。
6. 当前 `streamJsonPatchEntries` 原地应用修复保留，因它独立修复 live overlay 每 patch 深拷贝导致的 O(N²)；历史主路径不再依赖数万 patch。

### 错误与响应契约

结构化错误至少区分：

- `native_session_id_missing`
- `native_session_file_not_found`
- `native_session_permission_denied`
- `native_session_format_unsupported`
- `native_session_corrupt`
- `native_session_not_flushed`

错误响应携带 agent、session ID 后 8 位、候选 root（不暴露完整用户目录给远端客户端）、是否可重试和 Raw Logs process ID。解析器按行读取；文件尾部半行仅在运行中视为可重试，已完成 process 则报 corrupt。

### 其他 Agent 删除策略（2026-08-12 更新：不保留兼容层）

用户决策：**不对旧实现保留任何兼容层**。具体含义：

- **产品与运行时删除**：从 `CodingAgent` 可执行 enum、`default_profiles.json`、安装/可用性检测、MCP 分支、frontend 选择器/图标、文档导航、测试和依赖中移除 Amp、Gemini、OpenCode、Cursor Agent、Qwen Code、Copilot、Droid。
- **不设 legacy 读取边界**：`execution_processes.executor_action` 直接反序列化为 `ExecutorAction`（`ExecutorActionField::Other` 透传变体已删除）；含已删除 Agent 的旧数据库行在查询时直接反序列化失败并显式报错，不提供只读 DTO。
- **配置不宽容**：用户 `profiles.json` 含未知 executor 键时按解析失败处理（回退默认配置并记录错误），不做逐键过滤。
- **错误路径**：`sessions.executor` 字符串无法解析为 `BaseCodingAgent` 时，会话历史接口返回 `native_session_format_unsupported`（500），不再使用独立的 `legacy_executor_unsupported` 错误码。
- **旧前端实现不维护**：`useConversationHistoryOld`（VirtualizedList/TaskAttemptPanel 链路）不做新架构兼容改造；该路径对已完成进程不保证历史可读，随旧 UI 退役。

历史 migration 文件保留原样（属于历史记录，不是兼容层）。

### 生产环境检查

- **兼容性**：三 Agent 当前 session ID 已存在于 `coding_agent_turns`。新 API 按 Vibe session 聚合，避免 follow-up 重复。不保留 legacy executor 兼容层（旧数据显式报错）。现有 Raw Logs 面板保留。`shared/types.ts` 只能通过 `pnpm run generate-types` 更新。
- **失败行为**：文件读取单次超时 2 秒；运行中 `not_flushed` 使用 100/250/500/1000 ms 有界重试，总预算 2 秒；确定性格式错误不重试。文件读取是只读且幂等。无静默 fallback；用户可打开 Raw Logs 诊断。
- **可观测性**：记录 agent、hash 后 session ID、fingerprint、解析耗时、entry 数、cache hit、错误码；不记录 prompt/content 或完整 home path。指标建议：`native_history_load_seconds`、`native_history_errors_total{agent,code}`、`native_history_entries`、`native_history_cache_hit_total`。P95 > 1 s 或 5 分钟内格式错误率 > 1% 告警。
- **上线与恢复**：先加 provider/API 与 shadow comparison，再切换完成态读取，再删除其他 Agent。灰度开关按 agent 控制。发现 entry 数/最终消息不一致、格式错误率超阈值或 follow-up 重复时关闭对应 provider；回滚只恢复旧 UI 路由，Raw Logs 始终可用。删除 executor 的提交最后落地，回滚时恢复代码和默认 profile，不修改用户原生文件。

## 计划

### Phase 1 - 定义契约与三种黄金 fixture

- 改动：新增统一 provider trait、错误模型和 session 级输出 DTO；从脱敏后的真实 PI/Codex/Claude Code 文件制作最小 fixture，覆盖分支、compaction、tool、thinking、sidechain、尾部半行。先不接生产路由。
- 涉及模块：`crates/executors/src/history/`（新增）、三种 executor 子模块、`crates/server/src/bin/generate_types.rs`。
- 工作目录：仓库根目录。
- 验收命令：`cargo test -p executors native_session_history -- --nocapture && pnpm run generate-types:check`
- 预期证据：三种 fixture 重复解析输出稳定；没有 delta；session ID 校验失败和尾部半行分别产生预期错误码；生成类型无漂移。

### Phase 2 - 实现 PI provider

- 改动：复用 PI SessionManager 的 active branch/compaction 语义，完成 PI entry 映射、路径校验和性能基准。若 Rust 侧无法直接链接官方 SessionManager，先以受版本约束的子进程/Node helper 调用已安装 PI 包；不得复制其树/compaction 算法。
- 涉及模块：`crates/executors/src/executors/pi/`、`crates/executors/src/history/`。
- 工作目录：仓库根目录。
- 验收命令：`cargo test -p executors pi_native_history -- --nocapture && cargo test -p executors --release pi_native_history_benchmark -- --ignored --nocapture`
- 预期证据：问题样本恢复 1 user、45 assistant、76 tool result 的原生消息集合（映射成更多 block entry 时有可解释关系）；active branch 无旁支重复；758 KB fixture 的 dev P95 < 500 ms。

### Phase 3 - 实现 Codex 与 Claude Code provider

- 改动：Codex 复用 `RolloutRecorder`/`RolloutItem`；Claude Code 在现有 `ClaudeJson`/normalizer 状态机上增加 batch reader，并实现主链、sidechain 和 replay 过滤。
- 涉及模块：`crates/executors/src/executors/codex/`、`crates/executors/src/executors/claude.rs`、`crates/executors/src/history/`。
- 工作目录：仓库根目录。
- 验收命令：`cargo test -p executors native_session_history_codex -- --nocapture && cargo test -p executors native_session_history_claude -- --nocapture`
- 预期证据：Codex function call/output、reasoning、assistant 去重正确；Claude sidechain/replay 不进入主历史，tool call/result 状态与现有 live UI fixture 一致。

### Phase 4 - 接入 session 级历史 API 与 live overlay

- 改动：新增 session 级 history WS/API；服务只解析一次 native session，合并 setup/cleanup；前端 historic path 改为 session 级读取，running path 保留 overlay 并在完成后整体替换。删除 DB fallback raw normalizer，不再显示无限 Loading。
- 涉及模块：`crates/services/src/services/container.rs`、`crates/server/src/routes/`、两个 `useConversationHistory` 实现、`streamJsonPatchEntries`。
- 工作目录：仓库根目录。
- 验收命令：`pnpm run check && cargo test -p services native_history -- --nocapture && cargo test -p server native_history -- --nocapture`
- 预期证据：initial + 两次 follow-up 的历史无重复；完成时 overlay 被替换而非追加；文件缺失 2 秒内结束 Loading 并显示结构化错误；Raw Logs 可打开。

### Phase 5 - 影子对比与真实样本验收

- 改动：在不影响 UI 的 shadow 模式读取三种原生历史，与 live normalizer 的最终 user/assistant/tool 边界比较；记录差异指标，不记录内容。用本问题 PI 任务作为固定回归样本。
- 涉及模块：services 可观测性、集成测试 fixture、开发诊断脚本。
- 工作目录：仓库根目录。
- 验收命令：`cargo test --workspace native_history -- --nocapture && pnpm run check`
- 预期证据：三 Agent fixture 最终消息和工具边界一致；问题 PI 任务接口首条响应 < 1 s、缓存命中 < 100 ms；无 `Lagged` 驱动的历史缺失。

### Phase 6 - 移除其他 Agent 运行时支持

- 改动：从 executor enum/default profiles/MCP/前端/文档/依赖删除七种 Agent，不保留 legacy DTO 或宽容解析；更新 `docs/supported-coding-agents.mdx`、agent 配置文档和 `docs/docs.json`。删除不再可达的实现与测试，不保留空壳运行时。
- 涉及模块：`crates/executors`、`crates/services`、`crates/server`、`frontend`、`shared/types.ts`（生成）、`docs`。
- 工作目录：仓库根目录。
- 验收命令：`pnpm run generate-types && pnpm run check && cargo test --workspace && rg -n 'AMP|GEMINI|OPENCODE|CURSOR_AGENT|QWEN_CODE|COPILOT|DROID' crates frontend/src shared/types.ts docs/docs.json docs/supported-coding-agents.mdx`
- 预期证据：产品选择器和默认 profile 只含 PI/Codex/Claude Code；搜索仅命中历史 migration 或明确归档文档；含 legacy executor 的数据库行在读取时显式报错，服务本身可启动。

## 测试

### 最小运行检查

每个 provider 至少有一条“原生文件 → `Vec<NormalizedEntry>`”纯函数测试，断言：

- 最终 user/assistant/tool 数量和顺序；
- delta/control/隐藏 sidechain 未出现；
- 最终 assistant 文本等于原生物化记录，不由 delta 拼接推断；
- session ID/cwd/path 校验有效；
- 同一 fingerprint 结果确定且缓存命中；
- 解析器不修改原生文件（测试前后 hash 相同）。

PI 增加树/compaction fixture；Codex 增加 response item/event message 去重 fixture；Claude Code 增加 parentUuid、sidechain、replay 和 tool pair fixture。

### 集成与回归

- session 含 initial + follow-up + review 时只读一个 native file，user turn 不重复。
- setup/cleanup raw entries 按 process 时间定位，不打乱 native 对话。
- live 运行中刷新页面，先恢复 native 已落盘部分，再显示当前 overlay；完成后条目数不翻倍。
- 删除/改权限/截断原生文件，页面停止 Loading，显示正确错误码，不触发 raw normalization。
- fixture 数据库含 legacy executor 数据时，服务可启动；读取这些行返回显式错误而不是 panic 或静默兼容。
- 全量命令：`pnpm run check && cargo test --workspace && pnpm run generate-types:check`，不得以“跳过测试”声明通过。

## 备注

### 已确认决策

- 产品只支持 PI、Codex、Claude Code。
- 历史以对应 Agent 的原生物化文件为正常事实源。
- 其他 Agent 删除运行时支持，不再投入历史 provider。
- 原生文件失败时显式报错并提供 Raw Logs，不静默回退 raw-log 重放。
- 2026-08-12（用户）：不对旧实现保留任何兼容层——`ExecutorActionField::Other` 只读透传、profiles.json 宽容解析、`legacy_executor_unsupported` 错误码均已移除，旧数据显式报错。
- 2026-08-12（用户）：项目未投入生产使用，旧数据不需要保留。
- 2026-08-12（用户，更正）：删除的是 Beta 工作区页面（`/workspaces` 全套，含 pages/ui-new、NewDesignLayout、NewDesignScope 及级联孤儿组件约 100 个文件）；task 为主的页面（ProjectTasks、FullAttemptLogs 等）保留，其中因历史加载方式变更而需兼容的部分（`useConversationHistoryOld` 及逐进程 WS 重放链路）已删除，VirtualizedList 改用原生历史实现（`hooks/useConversationHistory/useConversationHistory.ts`，由 ui-new 版本迁入）。ui-new 目录仅保留被旧树实际引用的共享原语/hooks/settings 组件（50 个文件）。ProjectTasks 中的 Beta 邀请弹窗与 beta 重定向逻辑已摘除。

### 授权门槛与开放问题

- **新增依赖（待实施前确认）**：PI provider 是否能从 Rust 直接链接官方包；若需要 Node helper，只允许调用已安装的 `@earendil-works/pi-coding-agent`，不得新增另一套 parser 依赖。新增依赖前需人工确认。
- **数据库 schema**：推荐方案不新增表、不需 migration。若实施发现必须保存原生文件绝对路径或 leaf ID，先回到方案评审，不得直接加 schema。
- **Claude Code 格式稳定性**：本地样本不足，Phase 1 必须补充脱敏真实 fixture，确认主链/sidechain 规则后才能进入 Phase 3。
- **remote deployment**：本期明确不支持跨机器读取；若 remote 是必须场景，需要单独方案定义 agent host 上的受控文件读取 API。
- **PI active leaf**：session 文件 header 不直接保存 leaf 时，应调用 SessionManager 读取其当前 leaf，禁止自行用“最后一行”猜测。

### 风险与回滚

- Agent 私有格式升级可能使 parser 失效；通过版本 fixture、结构化错误和 agent 级开关隔离。
- 原生文件可能被用户删除；这是可见的数据生命周期，不由 Vibe 隐藏或重建。
- 删除 executor 是破坏性产品变更；用户已确认不保留 legacy 兼容层（2026-08-12）。回滚恢复代码/default profiles/types，不触碰原生文件和 raw logs。
- 当前 `streamJsonPatchEntries` 的原地 patch 修复独立成立并保留；已验证会丢数据的服务端 snapshot-fold 实验已撤回，不作为本计划基线。
