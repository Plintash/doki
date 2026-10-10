# Proposal

## Why

任务一多、项目一多，侧栏就不再是一张能找到东西的清单。今天只有"项目 / 更新时间"两种分组，
每行第二行显示的是分支名——而在 always-worktree 的工作流里，那个分支名是
`waku/<首句前六个词的 slug>`（`crates/waku-core/src/worktree.rs:226`），跟第一行标题重复；
更糟的是 `sessions` 表没有 workspace 列、`list_projection()` 把 workspace 归零成 `Local`
（`crates/waku-protocol/src/model.rs:1106`），所以重启之后每一行显示的分支其实是**项目当前
分支**。结果是三个问题都答不出来：哪条在等我、哪条在跑、哪条已经可以收起来。

量级（本机 `~/Library/Application Support/Doki/app.db`，`select count(*) from sessions` 与
`select length(data) from session_details`）：41 个任务里 9 个的细节记录超过 2 MB，最大的
3.0 MB，而 1747 条消息正文合计只有 730 KB。任务早就长成了龙，标题却停在第一句话。

## What Changes

- **新增状态视图**：不按项目分组，按 `需要你 / 进行中 / 最近` 三档排列；`需要你` 覆盖
  `Waiting` 与 `Failed`，按"进入阻塞的时长"降序；后两档按 `last_reply_at` 降序。扁平列表因此
  不再需要项目内的"3 天窗口 + 每次露出 30 条"启发式。原来的项目视图与更新时间视图都保留。
- **每行第二行只描述"现在"**：阻塞原因 → 忙且有 provider plan step → 忙且有 objective →
  否则保持今天的内容（**闲置行不替换**，也不显示"working"之类的降级文案）。行上不再显示一条
  任务并不在的分支。
- **行工具提示**：标题、objective、阻塞原因或当前 step、项目、已知分支、事实行（轮数 / 改动
  文件 / 新鲜度），单卡片不分栏、事实右对齐塞进空位。悬停与键盘焦点显示同一张卡片，内容全部
  来自客户端已持有的值。
- **Waku 拥有的 objective**：新增字段，由 daemon 在 turn 结算后触发、在任务**自己的 Pi 实例里**
  生成——同一条 RPC 管道上发一个扩展命令，扩展用 `ctx.modelRegistry.complete(...)` 做一次嵌套
  模型调用，再用 `pi.appendEntry` 把结果发回会话事件流。不起第二个进程、不建会话、不写
  transcript、不阻塞任何 turn；有用户 goal 时以 goal 为准。
- **新增几条窄字段**：阻塞起始时间与原因（"最久未处理在前"和行上的原因需要可持久、可跨客户端的
  来源）、轮数与改动文件数（卡片的事实行需要，列表投影里没有 transcript）。这些字段由 daemon
  拥有，任何客户端保存都不得清空它们。
- **归档层 1**：归档/取消归档是显式动作，标记持久化，列表默认不显示，可切换查看；不删数据。

非目标：归档的存储瘦身与磁盘清理（层 2/3）、worktree 孤儿清理、任务看板、为 Claude/Codex 生成
摘要、改动标题。

## Capabilities

### New Capabilities

- `task-sidebar`: 侧栏作为任务调度面——三种视图、状态视图的分档与排序、第二行的"现在"规则、
  归档档位的位置、行工具提示的内容与可达性。
- `task-digest`: Waku 拥有的 objective——字段归属与解析优先级、生成时机与节奏、旁路调用形状、
  解析期质量规则、失败降级、以及"客户端保存不得清空"的归属规则。
- `task-archive`: 任务的归档状态——显式动作、持久化、不删数据、可见性与恢复、检索仍可达。

### Modified Capabilities

无。`specs/pi-provider` 描述的是 RPC 会话传输，而摘要的旁路调用刻意绕开那条会话（它不驱动
RPC、不写任务会话），因此该 capability 的行为没有变化。

## Impact

- `src/app/sidebar.rs`：状态视图分档与区块、行指纹与行事实缓存、第二行规则、工具提示、归档过滤。
- `src/app/runtime.rs`：catalog 合并白名单（新字段必须在这里存活）、从 catalog 应用 objective。
- `src/app/streaming.rs`：在状态进入 `Waiting`/`Failed` 时记录阻塞起始时间与原因。
- `crates/waku-client/src/persistence.rs`：视图选择新增枚举值（唯一一处定义），保持既有
  `sidebar_grouping_chosen` 语义；归档可见开关同样持久化。
- `crates/waku-protocol/src/model.rs`：`AgentSession` 新增 `objective`、`blocked_since`、
  `blocked_reason`、`turn_count`、`changed_files`；`list_projection()` 保留它们。
- `db/schema.ts` + `db/migrations/0004_*.sql` + `crates/waku-core/build.rs`：`sessions` 新增
  `archived_at` 等窄列（迁移前缀必须连续）。
- `crates/waku-core/src/persistence.rs`：SELECT/行元组、`session_skeleton`、`UPSERT_SESSION`
  的位置参数与列清单。
- `crates/waku-core/src/daemon.rs`：结算触发、静默门与频率封顶、触发命令的下发、daemon 拥有的
  字段在保存路径上的保留（沿用 `preserve_daemon_checkpoints` 的先例）、catalog revision 发布。
- `crates/waku-core/src/driver/pi.rs`：`entry_appended` 的处理（过滤自定义类型与版本）。
- `resources/pi-extensions/`：新的 Pi 扩展（`waku:digest` 命令 + 嵌套模型调用 + `appendEntry`），
  随包加载，参照现有的 `resources/computer-use/pi-extension.ts`。
- `crates/waku-core/src/server.rs`：`SessionCatalogEntry` 携带新字段（用于等值检测），以及
  daemon 侧的发起点。
- `locales/{app,zh-CN,ja}.yml`：新视图、三档、归档档位与归档动作的文案（键集合一致性已有测试）。
- `packages/waku-client/src/generated/`：ts-rs 绑定重新生成；`apps/web` 与 `apps/mobile` 需要
  类型检查通过，且**不得**在保存时清空新字段。
