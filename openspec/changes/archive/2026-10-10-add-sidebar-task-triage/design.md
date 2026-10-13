# Design

## Context

动机见 `proposal.md - Why`。与本设计相关的现状（行号为本次校对后的位置）：

- 行模型是 `SidebarRow`，分组由 `SidebarGrouping`（`Project` / `Updated`）驱动，**只有一处
  定义**：`crates/waku-client/src/persistence.rs:46`（随偏好文档持久化，另有
  `sidebar_grouping_chosen` 记录"用户是否选过"）。`SidebarGroup` 是
  `Copy + Hash + Eq` 的稳定身份，折叠状态是**内存里的 HashSet**、不持久化。
- 列表是虚拟化的 `list()`；卡片高 51px、行间距 1px（行高 52px）。行快照由
  `sidebar_rows_cached` 按指纹重建，指纹只混入整型事实（分组、排序、每会话 id/project、
  时间戳、项目重看窗口、露出计数），**不含 `status`**，且必须保持零分配。
- **第二行今天几乎恒定**：`sessions` 表没有 workspace 列（`db/schema.ts`），
  `list_projection()` 把 workspace 归零成 `Local`（`crates/waku-protocol/src/model.rs:1106`），
  于是 `persisted_sidebar_branch_label(&session.workspace)` 落到项目当前分支。而在
  always-worktree 的工作流里，worktree 分支名是 `waku/{prompt 前 6 个 ASCII 词的 slug}`
  （`crates/waku-core/src/worktree.rs:226-240`，非 ASCII 首句会退化成 `waku/new-worktree`）
  ——这第二行要么是标题的近重复，要么是个跟任务无关的常量。
- 列表投影是刻意轻量的：`list_projection()` 清空 `messages` / `transcript_blocks` /
  `turns` / `queued_messages`，并把 `thread_goal`、`context_usage`、`provider_cursor` 归零
  （`model.rs:1100-1131`）；daemon 只保留最近 24 条会话的 transcript
  （`RESIDENT_TRANSCRIPT_WINDOW`，`crates/waku-core/src/daemon.rs:35`）。
- `SessionCatalogEntry`（`crates/waku-core/src/server.rs:149-176`）是 daemon **本地**的等值
  账本：它只用来检测"有没有变"，线上广播的只有 `TaskStateChanged { revision }`，客户端收到
  后重新 `LoadTaskState`（`crates/waku-client/src/client.rs:327`）。桌面端合并 catalog 行时
  走字段白名单（`src/app/runtime.rs:139-155`）。
- provider 进程由 daemon 拥有（`daemon.rs:46`），Pi 会话是 daemon 通过 **RPC** 驱动的长驻子进程
  （`crates/waku-core/src/driver/pi.rs` 发送 `prompt`/`steer`/`set_thinking_level` 等命令），因此
  "复用那个实例"是现成的：Pi 的扩展面提供 `ctx.modelRegistry.complete(model, context, options)`
  （provider-neutral、用实例自己的凭据解析，`dist/core/model-registry.d.ts`）、
  `pi.appendEntry(customType, data)`（不进模型上下文，且会发 `entry_appended` 会话事件，
  `docs/json.md:122`），而扩展命令在流式期间也能立即执行、响应为 `disposition: "handled"`。
  Waku 已经在传 `--extension`（`pi.rs:328/412`），并已随包发过一个 Pi 扩展
  （`resources/computer-use/pi-extension.ts`）。
- **但 daemon 今天不解释事件**：per-session 线程只把 `WireDriverEvent` 转发出去
  （`daemon.rs:816-831`），会话状态由客户端应用并通过 `SaveTaskState` 推回
  （`daemon.rs:353-428`）。daemon 已有关键的"不可被客户端覆盖"先例：
  `preserve_daemon_checkpoints`（`daemon.rs:952+`，配套的 stale 合并见 `daemon.rs:923-947`）。
- provider 的标题治理写在 `docs/titles.md`（provider 自己命名优先，Waku 只为 Codex 生成
  标题）；本设计不碰标题。
- `ActivityKind::Plan` 是**按工具名**判定的（`model.rs:2088-2094` 匹配
  `todo|todowrite|updateplan|plan`），因此 Claude 的 `TodoWrite`、Codex 的 plan item、
  DeepSeek 的 `todo/write`、ACP 的 `plan` 都会成为 Plan 活动；**Pi 不自带** plan/todo 工具
  （内置只有 read/bash/powershell/edit/write/grep/find/ls 与两个内置扩展工具
  codemode/tool_search），但扩展或 MCP 提供的同名工具仍会被归类为 Plan。
- 悬浮提示的机制：`.tooltip(Fn(&mut Window, &mut App) -> AnyView)` 存在，但 GPUI 把它定义成
  **鼠标悬停**触发；`src/ui/menu.rs` 的 `popover()` 靠鼠标按下或 Enter/Space 打开，它用
  `deferred(...)` 绘制（因此能逃出侧栏的 `overflow_hidden`），层级由 deferred priority 决定。
  `ContextMenuHandle` 的约定是"每个菜单位点一个"。
- 迁移链路：`db/schema.ts` → `bun run db:generate` → `db/migrations/000N_*.sql` →
  `crates/waku-core/build.rs` 按数字前缀嵌入（要求从 `0000` 连续）→ `apply_migrations`
  逐条打标、幂等。三份 locale 的键集合一致性已经有测试（`apps/web/src/lib/i18n-core.test.ts`）。

## Goals / Non-Goals

**Goals:**

- 让"哪条在等我、哪条在跑、哪条是什么"在不打开任务时可读，且不引入 render 路径上的 I/O。
- 把摘要层放在 daemon：一次计算，所有客户端与重启后都可用，且不会被任何一次客户端保存抹掉。
- 让归档成为"从视野里拿走"，且它的状态不会被旧客户端写没。

**Non-Goals:**

- 归档的存储瘦身与磁盘清理（层 2/3）、worktree 孤儿清理、任务看板。
- 改动标题的任何行为（含 `auto_title`）。
- 为 Claude / Codex 生成摘要：本期只覆盖 Pi（无 plan、且是主要 provider）。其它 provider 靠
  自己的标题与 plan step，缺摘要时按"无摘要"渲染。
- 让摘要产生"当前步骤"：见 D4 —— 摘要在轮结算后产生，天然描述的是刚过去的那一轮，不能冒充
  当前进度。

## Decisions

### D1. 摘要是新字段，不是对标题的修改

`AgentSession` 增加 `objective: Option<String>`。标题保持稳定，因为它是用户认路的锚；会变的
标题比不完美的标题更难找。

- 替代 A（改进标题）：会与 provider 标题互相覆盖，且每次改动都让用户失去锚点。
- 替代 B（复用 `auto_title`）：`docs/titles.md` 规定该字段是 provider 标题与本地 fallback 的
  共用槽位，塞入第二种语义会让两者互相覆盖。
- 替代 C（复用 `thread_goal`）：它是 provider 拥有的账本（Codex 专用，带 token/时间预算）。
  改为"读它、不写它"。

### D2. 优先级：用户 goal > provider plan > 摘要，且在 daemon 里解析一次

`objective` 存的是**解析后的展示值**：有用户 goal 时就是 goal，否则是生成的摘要。解析只发生
在一处（daemon），客户端只渲染收到的值——否则两个客户端会对同一任务显示不同目标，并且客户端
可能把本地解决结果写回覆盖 daemon。

- 替代：让每个客户端各自解析（`thread_goal` 不在列表投影里，客户端只能看到摘要，见 D9）。

### D3. objective 说结果，且在解析时强制

objective 用一句完成时态的结果陈述；解析器拒绝含路径分隔符、文件扩展名或符号样式 token 的
输出，保留上一版文本（测试覆盖恶意输出）。step 允许指名道姓。

### D4. 第二行只描述"现在"，闲着的行不动

第二行按此顺序决定：**阻塞原因 → 忙且有当前轮的 plan step → 忙且有 objective → 保持今天的
内容**。闲置任务不显示 objective：那会把"过去在做什么"伪装成"现在"（也正是 owner 说的
"没有 step 的 provider 原来怎样现在怎样"）。摘要不再产出 `step` 字段，因此不存在"旧 step 冒充
当前步骤"的问题；`step` 这个位置只留给 provider 自己的、属于当前轮的 plan 活动。

### D5. 生成在任务自己的 provider 进程里，不做第二个进程

摘要由 Waku 随包发的一个 Pi 扩展在同一实例内完成：

- **触发**：daemon 往已经打开的那条 RPC 管道发 `{"type":"prompt","message":"/waku:digest"}`。
  扩展命令不算一轮（响应 `data.disposition == "handled"`），所以它既不起 run、不写 transcript，
  也不会增长任务上下文；实现要把这个字段当断言，而不是当作可以忽略的返回值。
- **执行**：扩展用 `ctx.sessionManager.getBranch()` 读活分支、自己裁一个有界输入，用
  `ctx.modelRegistry` 选模型（便宜档优先，`hasConfiguredAuth()` 过滤无凭据的，否则用会话自己的
  模型），调用 `ctx.modelRegistry.complete(...)`（带超时），把结果
  `pi.appendEntry("waku:digest", {...})`。
- **回传**：`entry_appended` 是同一条 RPC 流上的会话事件，driver 过滤自定义类型后交给同一套纯函数
  解析、校验、走滞后门，再写回会话并推一次 catalog revision；usage 随条目带回，接进现有计量。
- **失败**：handler 什么都不 append（或 append 一个客户端忽略的标记），旧值保留、静默；
  扩展内部任何异常都不得影响会话。

- **替代 A（第二个 `pi --print --fork …` 进程）。** 需要猜 `--fork`/`--session-id`/`--session-dir`
  的组合语义、自己管临时会话目录、并面对未能证实的模型/凭据继承；在本机验证中它还会让一次性调用
  暴露在 CLI 的标志集上。既然实例已经在手，没有必要。
- **替代 B（把 prompt append 进 live 会话）。** 它会成为会话的真实 user turn：agent 看得到、上下文
  永久增长、resume 后仍在；流式期间还会被当作 steering。扩展命令路径没有这个代价。
- **替代 C（自建输入的一次性进程）。** 丢掉复用实例的全部好处，还要自己重建 transcript。

### D6. 触发与节奏

结算触发（不是活动、不是时间）；结算后静默门 15s；每任务每小时 ≤6 次；60s 超时；不做历史
回填（安装前已结算的轮次永不生成）；resume/打开不触发。数字写进 spec，方便验收断言。

### D7. 稳定性优先的滞后门

改写前先做一次廉价的显著词重叠检查：与已存 objective 重复度足够高就保留旧文本。同一个阈值
也用来拒绝"与标题重复"的 objective。这样"漂移检测"是改写决策的副产品，不需要额外判断调用。

### D8. 视图是第三个分组 + 指纹与行事实缓存

`SidebarGrouping` 增加 `Status` 变体（Project / Updated / Status），状态视图用新的
`SidebarGroup` 身份（`NeedsYou` / `Running` / `Recent` / `Archived`）。必须同时改两处：

- **快照指纹**加入 `status`（状态视图下）、归档标记、归档可见开关、当前视图——只做整型混合，
  **不得**把 digest 文本塞进指纹；否则任务从 Idle 变 Waiting、或归档，行不会被重新分区。
- **行事实缓存**：第二行与卡片要的数据（objective、原因、轮数、改动数）来自一个"id → 行事实"
  的缓存，在会话数据变化时刷新；行构建器不得扫描 `transcript_blocks`（`AGENTS.md` 的每帧
  规则；现有 `sidebar_rows_cached` 就是这个模式）。

### D9. 新增的窄字段，以及为什么必须有

| 字段 | 谁写 | 为什么不能在渲染时算 |
|---|---|---|
| `objective` | daemon（解析后的展示值） | 列表投影里没有 goal，也没有 transcript |
| `blocked_since` | 进入 `Waiting`/`Failed` 的转换点（客户端） | "最久未处理在前"需要可持久、可跨客户端的顺序；`last_reply_at` 只在提交与结算时更新 |
| `blocked_reason` | 同上 | 待批准的原因今天只存在于运行时内存，重启/其它客户端就没了 |
| `turn_count` / `changed_files` | daemon（结算时维护） | 投影里 `turns` 是空的，改动数藏在 checkpoint 里 |
| `archived_at` | 显式动作（见 D11） | 列表过滤必须只扫窄行 |

上下文占用不进这一组：它天然是活的，卡片只在客户端已持有它时显示。

### D10. daemon 必须能发布，而不仅是被动记账

今天的链路只有客户端保存才触发 catalog 变更，daemon 的转发线程不解释事件。因此：转发线程识别
`TurnFinished`（settlement）→ 调度生成 → 写 store → **推一次 catalog revision**。发布按每次
生成合并、与旧值相同就不发，且更新 objective 不得 bump `updated_at`。

### D11. daemon 拥有的字段不可被覆盖；归档是显式动作

- `objective` / `blocked_since` / `blocked_reason` / 计数按 `preserve_daemon_checkpoints` 的
  先例在 `SaveTaskState` 的替换与 stale 合并两条路径上保留。
- `archived_at` 不靠"保存时携带"来设置：归档/取消归档是显式动作，普通保存不得清除或设置它，
  因此旧客户端（甚至不认识该字段的构建）也不会把归档状态写没。
- 桌面端 catalog 合并白名单（`src/app/runtime.rs:139-155`）必须带上这些字段，否则"daemon 生成
  了但列表行收不到"。

### D12. 工具提示：纯函数 + 两个表面各用各的句柄

卡片由一个纯函数构建，输入只有客户端已持有的值（列表条目、objective、原因、项目、缓存的
分支标签），签名里**不允许**出现 daemon client / store / path。悬停用 `.tooltip(...)`
（GPUI 原生延迟与层级）；键盘焦点用每行**第二个** `ContextMenuHandle`（`session-tooltip-{id}`）
配 `popover()`，在 focus/blur 上开关——不能复用行已有的上下文菜单句柄，否则两者会互相打开。
`src/ui/tooltip.rs` 只有文本构造器，卡片作为独立 view 提供，并在该文件的文档注释里说明分工。

### D13. 迁移站点与回滚

新增列必须同时改动：`db/schema.ts`、生成的 `db/migrations/0004_*.sql`（前缀必须连续）、
`SELECT` 列表与行元组映射、`session_skeleton` 的字段填充、`UPSERT_SESSION` 的列/更新列表与
位置参数、`list_projection()`、`SessionCatalogEntry`、桌面 catalog 合并白名单。回滚是数据保全
的：旧二进制只忽略新列，但**必须验证**它的 `load`/`save` 能容忍多出来的列（加一个降级测试），
否则回滚会把归档意图变成不可见而不是保留。

## Risks / Trade-offs

- **[扩展与 app 一起版本化，条目 schema 是唯一的契约]** → 条目带 `v` 字段，客户端忽略不认识的
  版本与畸形 payload；命令名带命名空间。
- **[扩展抛错不能影响会话]** → handler 内部全部包住，失败只体现为"什么都没 append"。
- **[catalog 广播成本]** → 每次生成只发一次 revision、值不变不发、`updated_at` 不动；每任务的
  频率由 D6 封顶。
- **[objective 抖动]** → D7 的滞后门 + 标题重复拒绝。
- **[行快照不重新分区]** → D8 的指纹改动 + "翻转一个任务的状态会重新分区"的测试。
- **[闲置任务被摘要淹没]** → 不做回填（D6），且闲置行不显示 objective（D4）；归档是配套收纳。
- **[状态视图需要新的持久化枚举值]** → JSON 按名字序列化，旧值不受影响；`sidebar_grouping_chosen`
  语义不变。

## Migration Plan

1. `db/schema.ts` 增加列 → `bun run db:generate` → 确认 `crates/waku-core/build.rs` 嵌入且前缀
   连续（新文件必须是 `0004_*.sql`）。
2. 所有新增协议字段都是可选且带默认值：旧记录解码为零摘要/未归档；三份 locale 的键集合测试
   保持通过。
3. 视图偏好新增枚举值向后兼容（按名字序列化）。
4. 回滚：代码回退即可；另加一条"新库 + 旧 load/save"的测试，证明多出来的列不会让旧二进制
   报错或清空数据。
5. 顺序：先落协议/存储与 daemon 生成，再落状态视图与卡片，最后落归档——每一步都能单独验证。

## Open Questions

- 是否为 Claude 这类"已有 provider 标题 + plan"的 provider 也生成 objective：目前判断不需要，
  实现后按实际观感再定，不影响本设计的结构。
- 卡片是否加入"最后一条用户消息"：字段现成，属于纯展示取舍，可后置。
- 是否把 `workspace` 也持久化到 `sessions` 行，让闲置行的分支不再退化成项目分支：这会顺手修掉
  一个现存缺陷，但属于独立改动；本期用"D4 不显示不知道的分支"绕开。
