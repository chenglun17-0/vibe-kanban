# Gitee Git Host 自动识别与 `ge` CLI 接入计划

- 状态：已实施，待真实 Gitee 凭据验收
- 当前阶段：代码、单元测试和静态检查完成；待真实仓库端到端验证
- 最近更新：2026-08-13
- 关联需求：根据仓库 remote URL 自动选择 GitHub `gh`、Gitee `ge` 或现有 Azure DevOps provider

## 概述

### 已确认决策

不新增 Git Host 平台设置。系统继续以仓库地址作为唯一选择依据：

- GitHub 地址使用 `gh`。
- Gitee 地址使用 `ge`。
- Azure DevOps 地址继续使用 `az`。
- 无法识别的平台返回 `UnsupportedProvider`。

这里的“仓库地址”不是注册仓库时缓存的一份固定字符串，而是每次 PR 操作已经解析出的**实际 target remote URL**。这样在存在 `origin`、`upstream` 或分支跟踪不同 remote 时，provider 与本次 PR 的目标仓库保持一致。

### 当前事实

当前代码已经采用 URL 自动识别：

- `crates/services/src/services/git_host/detection.rs` 识别 GitHub、GitHub Enterprise 和 Azure DevOps。
- `crates/services/src/services/git_host/mod.rs` 根据检测结果创建 `GitHubProvider` 或 `AzureDevOpsProvider`。
- `crates/server/src/routes/task_attempts/pr.rs` 在创建或关联 PR 前解析实际 target remote，再调用 `GitHostService::from_url`。
- `crates/services/src/services/pr_monitor.rs` 使用已保存的 PR URL 恢复 provider 并刷新状态。
- Gitee 地址目前会落到 `ProviderKind::Unknown`，最终返回 `UnsupportedProvider`。

因此本期不需要数据库 migration、`Repo`/`UpdateRepo` 字段、共享设置类型或仓库设置 UI。核心工作是扩展 URL 检测并新增完整的 Gitee provider。

### 关于 `ge`

本计划使用社区项目 [`gitee.com/imjoey/ge`](https://gitee.com/imjoey/ge) 作为 Gitee 外部 CLI：

- 可执行文件：`ge`
- 认证：`GITEE_TOKEN`、`GE_TOKEN` 或 `ge auth login`
- 创建命令：`ge pr create -R owner/repo -H head -B base -t title -F body-file`
- 与 `gh`/`az` 相同，`ge` 由用户自行安装，不加入 Cargo/pnpm 依赖，也不由应用保存 token。

实施前必须锁定最低支持版本，并验证 PR 命令的非交互参数、JSON 输出和退出码。不能把中文或英文的人类可读输出作为唯一解析协议。若 `ge` 缺少稳定机器输出，应先比较“创建后使用 JSON 查询定位 PR”与“直接调用 Gitee OpenAPI”两个方案，再继续实施。

## 目标

1. 标准 Gitee HTTPS 和 SSH remote 自动识别为 `Gitee`。
2. GitHub 地址继续自动选择 `gh`，Azure DevOps 地址继续选择 `az`。
3. Gitee provider 完整实现现有 `GitHostProvider` 契约：创建 PR、查询状态、按分支列举、读取评论、列出打开的 PR。
4. 缺少 `ge` 或认证失败时返回 Gitee 专用、可操作的错误。
5. 后台 PR monitor 能根据 Gitee PR URL 自动恢复 Gitee provider。
6. 不引入任何平台选择设置或持久化配置。

## 非目标

- 不增加全局或仓库级 Git Host 平台选项。
- 不修改 `repos` 数据库 schema。
- 不内置、静默下载或自动升级 `ge`。
- 不在应用数据库或配置文件中保存 Gitee token。
- 不改变 git push 的 remote、分支跟踪或认证方式。
- 不新增 GitLab、Bitbucket 等其他 provider。
- 不实现 Gitee 独有的 review/test/merge 审批流程。
- 不支持无法从 URL 识别的平台伪装或任意自建域名；此类地址继续明确报 `UnsupportedProvider`。

## 需求与验收场景

- **R1 Gitee remote 检测**：`https://gitee.com/owner/repo.git`、`ssh://git@gitee.com/owner/repo.git` 和 `git@gitee.com:owner/repo.git` 均得到 `ProviderKind::Gitee`。
- **R2 GitHub 回归**：现有 GitHub.com、GitHub Enterprise、HTTPS 和 SSH fixture 仍得到 `ProviderKind::GitHub`。
- **R3 Azure 回归**：现有 `dev.azure.com`、`visualstudio.com`、`ssh.dev.azure.com` 和 `/_git/` fixture 仍得到 `ProviderKind::AzureDevOps`。
- **R4 精确匹配**：恶意或无关地址（如 `gitee.com.evil.example`、路径中包含 `gitee.com` 的其他 host）不能被识别为 Gitee。
- **R5 实际 target remote**：创建 PR 时根据目标分支解析出的 remote 选择 provider，而不是固定使用 `origin` 或 push remote。
- **R6 创建 PR**：分支推送成功后，Gitee provider使用 title、body、head、base 和 target repo 创建 PR，返回稳定编号和 URL。
- **R7 查询闭环**：创建或关联 Gitee PR 后，评论、打开 PR 列表和后台状态刷新均可用；合并后现有任务状态更新逻辑继续工作。
- **R8 可操作错误**：PATH 中没有 `ge` 时返回 `cli_not_installed + gitee`；认证失败时返回 `cli_not_logged_in + gitee`。
- **R9 安全性**：CLI 使用参数数组执行，PR body 通过临时文件传入；日志不记录 token、认证 header 或完整 body。

## 设计

### 1. Provider 类型与自动检测

在运行态 `ProviderKind` 增加 `Gitee`：

```rust
pub enum ProviderKind {
    GitHub,
    Gitee,
    AzureDevOps,
    Unknown,
}
```

`GitHostService` 增加：

```rust
pub enum GitHostService {
    GitHub(GitHubProvider),
    Gitee(GiteeProvider),
    AzureDevOps(AzureDevOpsProvider),
}
```

`GitHostService::from_url(url)` 仍是唯一 provider 工厂。所有 route 继续传入实际 remote URL，不增加配置参数。

### 2. URL 解析原则

当前检测大量使用 `contains`。新增 Gitee 时应同时把检测收敛为“先解析 host，再匹配 host”，避免以下误判：

```text
https://gitee.com.evil.example/owner/repo
https://example.com/gitee.com/owner/repo
```

需支持：

```text
https://gitee.com/owner/repo.git
git@gitee.com:owner/repo.git
ssh://git@gitee.com/owner/repo.git
```

检测规则：

1. 提取 HTTPS/SSH/SCP-like URL 的 host。
2. `gitee.com` 精确映射到 Gitee。
3. `github.com` 以及现有受支持的 GitHub Enterprise 规则映射到 GitHub。
4. Azure DevOps 保持现有 host 和 `/_git/` 规则。
5. 其余返回 `Unknown`。

不能仅使用 `url.contains("gitee.com")`。

### 3. Remote 与 PR URL 的不同入口

provider 检测有两类输入：

- **仓库 remote URL**：创建 PR、关联 PR、获取评论和打开 PR 列表。
- **PR URL**：后台 `pr_monitor` 查询已保存 PR 的状态。

两类输入都通过统一 host 解析选择 provider，但 owner/repo/PR number 的路径解析由对应 provider 负责。Gitee PR URL 的实际格式必须由 `ge` 返回 fixture 锁定，不能用宽泛的 `contains("pull")` 推断。

### 4. Gitee provider 模块

新增：

```text
crates/services/src/services/git_host/gitee/
  mod.rs
  cli.rs
```

`GeCli` 负责确定性工作：

- 从 Gitee remote 或 PR URL 解析 `owner/repo`。
- 检查 `ge` 是否在 PATH。
- 通过 `Command` 参数数组执行 `ge`，不经过 shell。
- 使用 `NamedTempFile` 传递 PR body。
- 解析稳定 JSON 为 `PullRequestInfo`、`OpenPrInfo` 和 `UnifiedPrComment`。
- 将退出码和错误映射为 `AuthFailed`、`CliNotInstalled`、`InsufficientPermissions`、`RepoNotFoundOrNoAccess` 或 `UnexpectedOutput`。

`GiteeProvider` 负责：

- `spawn_blocking` 边界。
- 仅对可重试错误做有界退避。
- 实现全部 `GitHostProvider` 方法。
- 返回 `ProviderKind::Gitee`。

需实现：

- `create_pr`
- `get_pr_status`
- `list_prs_for_branch`
- `get_pr_comments`
- `list_open_prs`

只实现 `create_pr` 不算完成，因为创建后的关联、评论和后台状态流程仍会失败。

### 5. 创建 PR 参数

目标 `owner/repo` 从 target remote URL 解析，head/base 沿用现有 `CreatePrRequest`：

```text
ge pr create
  -R owner/repo
  -H <head>
  -B <base>
  -t <title>
  -F <temporary-body-file>
```

草稿 PR 仅在最低支持版本确认 `--draft` 后透传；如果 Gitee 或 `ge` 不支持，则返回明确 capability 错误，不能静默创建普通 PR。

跨 fork 使用现有 `head_repo_url`，但不得直接复制 GitHub 的 `owner:branch` 语义。Phase 1 必须用真实或 mock 契约确认 Gitee 所需格式；未确认时明确标为不支持。

### 6. 错误和前端指引

不增加设置 UI，但现有错误界面需要认识 `ProviderKind::Gitee`：

- `frontend/src/components/dialogs/tasks/CreatePRDialog.tsx`
- `frontend/src/components/dialogs/CreateWorkspaceFromPrDialog.tsx`
- 其他显示 `ProviderKind` 名称的组件

行为：

- GitHub：保留现有 `gh` 自动 setup 流程。
- Gitee：显示 `ge` 安装文档、最低版本、`ge auth login` 和 `GITEE_TOKEN` 指引；不自动执行远程安装脚本。
- Azure DevOps：保持现有指引。

UI 不提供 token 输入框。后端错误返回前应过滤疑似 token 或认证 header。

### 7. 共享类型

只需将 `ProviderKind::Gitee` 生成到 `shared/types.ts`。不新增 `Repo`/`UpdateRepo` 字段。

必须通过：

```bash
pnpm run generate-types
```

禁止手工编辑 `shared/types.ts`。

### 8. 评论语义

若 `ge`/Gitee 只提供普通 PR 评论，没有 GitHub 式 review diff hunk，则仅映射可证明存在的数据。不能为了适配统一 DTO 而伪造 `path`、`line` 或 `diff_hunk`。

## 实施计划

### Phase 1 - 锁定 `ge` 命令契约

- 确认最低支持版本、许可证、安装来源和平台支持。
- 在隔离 Gitee 测试仓库验证 create/view/list/comments 的参数、JSON、退出码和认证错误。
- 验证草稿 PR 与跨 fork PR 能力。
- 保存脱敏 fixture，单元测试不依赖在线 Gitee。
- 若缺少稳定机器输出，暂停并重新评审 Gitee OpenAPI 方案。

验收证据：形成命令契约表；成功、认证失败、仓库不存在和输出异常 fixture 齐全；草稿及跨 fork 结论明确。

### Phase 2 - 扩展安全的 URL 自动检测

- `ProviderKind` 增加 `Gitee`。
- 将 remote/PR URL host 提取封装为可测试纯函数。
- 加入 Gitee HTTPS、SSH、SCP-like URL fixture。
- 加入 host 混淆和路径混淆的负例。
- 保持 GitHub Enterprise 与 Azure DevOps 测试通过。

验收命令：

```bash
cargo test -p services git_host::detection -- --nocapture
```

预期证据：标准 Gitee URL 识别为 Gitee；伪造 host 不误判；现有 GitHub/Azure fixture 无回归。

### Phase 3 - 实现 `GeCli` 和完整 Gitee provider

- 实现 `ge` 可用性检查、参数构造、临时 body 文件和 JSON parser。
- 实现全部 `GitHostProvider` 方法。
- 实现错误分类、脱敏和有界重试。
- 为 Gitee PR URL 增加状态解析 fixture。

验收命令：

```bash
cargo test -p services git_host::gitee -- --nocapture
cargo test -p services pr_monitor -- --nocapture
```

预期证据：fixture 覆盖创建、状态、分支 PR、打开 PR 和评论；测试证明无 shell 拼接；日志/错误不含 token 或 body。

### Phase 4 - 接入服务端调用链

- 在 `GitHostService` 注册 `GiteeProvider`。
- 保持 route 使用现有 target remote 解析，不新增设置读取。
- 覆盖创建 PR、关联已有 PR、获取评论和仓库打开 PR 列表。
- 后台 monitor 根据 Gitee PR URL 恢复 provider。
- 错误响应携带 `provider: gitee`。

验收命令：

```bash
cargo test -p server pr -- --nocapture
cargo test -p server repo -- --nocapture
cargo test -p services git_host -- --nocapture
```

预期证据：同一实例可根据不同仓库 URL 分别调用 `gh`、`ge`、`az`；不存在平台设置或数据库依赖。

### Phase 5 - 更新前端错误指引和共享类型

- 生成 `ProviderKind::Gitee` TypeScript 类型。
- 所有 provider 名称映射增加 Gitee。
- Gitee 缺少 CLI/未认证时展示安装和认证指引。
- 更新全部 locale：`en`、`es`、`fr`、`ja`、`ko`、`zh-Hans`、`zh-Hant`。
- 补充面向用户的 Gitee 集成文档，明确 `ge` 是社区外部依赖。

验收命令：

```bash
pnpm run generate-types:check
pnpm run check
pnpm run lint
```

预期证据：Gitee 错误不再显示笼统的 “Git host”；GitHub setup 弹窗无回归；所有 locale key 完整。

### Phase 6 - 端到端验证

| Remote/URL | 预期 provider | 预期行为 |
| --- | --- | --- |
| `https://github.com/o/r.git` | GitHub | 使用 `gh` |
| `git@github.com:o/r.git` | GitHub | 使用 `gh` |
| `https://gitee.com/o/r.git` | Gitee | 使用 `ge` |
| `git@gitee.com:o/r.git` | Gitee | 使用 `ge` |
| Azure DevOps URL | Azure DevOps | 使用 `az` |
| `https://gitee.com.evil.example/o/r` | Unknown | 拒绝，不调用 `ge` |
| Gitee URL，缺少 `ge` | Gitee | 显示安装指引 |
| Gitee URL，无效 token | Gitee | 显示认证指引 |

完整 Gitee 流程：

1. 自动识别 Gitee target remote。
2. 推送 source branch。
3. 创建 PR 并保存编号/URL。
4. 获取 PR 评论。
5. 重新进入工作区并关联已有 PR。
6. 外部合并 PR。
7. monitor 刷新状态并沿用现有逻辑更新任务。

全量门禁：

```bash
pnpm run check
pnpm run lint
pnpm run generate-types:check
cargo test --workspace
```

不得以只验证 `ge pr create` 成功或跳过后续查询流程来声明完成。

## 可观测性

记录非敏感信息：

- provider、操作名、CLI 版本、耗时、退出状态、是否重试。
- repo 仅记录内部 ID；不记录 token、完整 remote 凭据、PR body 或命令环境。
- `pr_created.provider` 应能产生 `Gitee`。

建议指标：

- `git_host_operation_total{provider,operation,result}`
- `git_host_operation_seconds{provider,operation}`
- `git_host_cli_error_total{provider,error_kind}`

## 风险与回滚

- **社区 CLI 稳定性**：`ge` 非官方。使用最低版本和 JSON fixture 约束，不解析本地化文本。
- **URL 误判**：host 解析错误可能调用错误 provider。使用精确 host 匹配和恶意负例测试。
- **自建 Gitee**：没有显式配置后，无法可靠识别任意企业域名。本期明确返回不支持，不能猜测。
- **跨 fork 差异**：Gitee head 语义可能不同于 GitHub，Phase 1 未确认前不宣称支持。
- **错误泄密**：CLI stderr 可能包含敏感信息，返回和日志前必须脱敏。
- **回滚**：移除 `Gitee` 工厂分支并恢复 Gitee URL 为 `Unknown`；无数据库 migration、设置字段或用户配置需要回滚。

## 实施记录

- 已按实际 target remote URL 自动选择 GitHub、Gitee 或 Azure DevOps，没有增加平台设置或数据库字段。
- `ProviderKind`、`GitHostService` 和共享 TypeScript 类型已增加 Gitee。
- 已新增 `GeCli`/`GiteeProvider`，最低支持 `ge v5.21.0`，覆盖创建、状态、分支 PR、评论、打开 PR 和 PR checkout。
- Gitee HTTPS、SSH、SCP-like URL 与 host 混淆负例均有单元测试。
- Gitee 跨 fork PR 明确拒绝；未套用 GitHub 的 head 语义。
- 已通过 `cargo test -p services git_host`、`cargo test -p server --lib`、`cargo clippy -p services -p server --all-targets -- -D warnings`、前端 typecheck/lint 和类型生成检查。
- 加载 `~/.zshrc` 后已使用 `ge v5.21.0` 和现有 Gitee token 完成只读在线验证：认证、PR list/view、comment list 均成功，`pr create --dry-run` 正确解析 title/head/base/body/draft，未产生远端写操作。
- 在线返回暴露了 Gitee 将未合并时间编码为 `0001-01-01T00:00:00.000Z`；解析器已将该零值视为 `None` 并增加回归测试。
- 真实创建、checkout 和 monitor 合并闭环仍未执行，因为这些操作会写入远端仓库或改变工作区，不能标记为完全验收。

## 完成标准

- URL 是唯一 provider 选择依据，代码中没有新增平台设置或持久化字段。
- GitHub、Gitee、Azure DevOps 根据实际 target remote 自动选择正确 CLI。
- Gitee provider 的创建、查询、评论、列表和 monitor 闭环均有测试证据。
- 缺少 `ge`、认证失败和不支持 URL 均显式暴露。
- 全量门禁通过且没有跳过测试。
