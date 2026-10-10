# Tasks

## 1. 协议、存储与归属

- [x] 1.1 在 `AgentSession` 增加 `objective: Option<String>`、`blocked_since: Option<u64>`、
      `blocked_reason: Option<String>`、`turn_count: u32`、`changed_files: u32`、`archived_at: Option<u64>`，
      并在 `list_projection()` 中保留它们；验证：`crates/waku-protocol/src/model.rs` 的测试覆盖
      "缺字段的旧 JSON 解码为零值"与"投影保留这些字段"，且 `display_title` 的解析顺序不变。
- [x] 1.2 在 `db/schema.ts` 的 `sessions` 表增加 `objective`、`blocked_since`、`blocked_reason`、
      `turn_count`、`changed_files`、`archived_at`（`objective` 是任务清单原本漏掉的：列表渲染不能
      去读 `session_details`，所以它必须是列），运行 `bun run db:generate`，确认
      `crates/waku-core/build.rs` 嵌入且数字前缀连续（新文件为 `0004_*.sql`）；验证：
      `crates/waku-core/src/persistence.rs` 的测试覆盖"新库建列"、"重复执行幂等"、
      "旧库升级后所有任务未归档且阻塞时间为空"。
- [x] 1.3 把新列接入读写全链路：`SELECT` 列表与行元组映射、`session_skeleton` 的字段填充、
      `UPSERT_SESSION` 的列/更新列表与位置参数、`SessionCatalogEntry` 的等值与 `From` 映射；
      验证：写入 → 重新加载 → 字段仍在的往返测试，且 catalog 测试断言 objective 无需 hydrate
      transcript 即可获得。
- [x] 1.4 实现 daemon 拥有的字段保留：在 `SaveTaskState` 的替换路径与 stale 合并路径上，对
      `objective`、`blocked_since`、`blocked_reason`、`turn_count`、`changed_files` 沿用
      `preserve_daemon_checkpoints` 的做法，`archived_at` 不接受普通保存的设置或清除；验证：
      三条回归测试——"不带 objective 的客户端推送不会清空已存 objective"、"归档后由旧投影保存
      仍然保持归档"、"离开阻塞状态时阻塞记录被清空"。
- [x] 1.5 让归档成为显式动作：daemon 侧新增 `Command::SetTaskArchived { archived }`（跟随
      `RemoveSession` 的形状，由 daemon 自己盖时间戳），并接入 `command_targets_runtime` 与
      `task_catalog_action`（因此命令本身就会推一次 catalog revision）；行菜单与命令面板的接线
      归 5.1；验证：单元测试覆盖"归档 → 重新加载 → 仍是归档"与"取消归档后回到未归档"，
      并断言普通保存不改变 `archived_at`。
- [x] 1.6 在 `src/app/runtime.rs` 的 catalog 合并白名单里加入新字段（含 `archived_at`），并让
      阻塞原因在 `src/app/streaming.rs` 进入 `Waiting`/`Failed` 时写入会话；验证：`src/app/tests.rs`
      现有的 catalog 合并测试扩展为"catalog 携带 objective 时合并后仍在"，状态转换的不变量
      由 `AgentSession::set_status`/`set_blocked_reason` 承担并有单测（盖一次章、重复上报不重置
      时钟、离开阻塞状态即清空、换阻塞状态保留时钟但丢弃旧原因）。
- [x] 1.7 重新生成 `packages/waku-client/src/generated/` 并用 TS 侧检查；计数器这两个字段最终
      定为 `Option<u32>`（`None` = 还不知道，与"0 轮"不是同一件事），因此 TS 里是可缺省的；
      验证：`bun run protocol:check` 报告绑定最新，`packages/waku-client`、`apps/web`、
      `apps/mobile` 三份 `tsc --noEmit` 全部通过。

## 2. 状态视图

- [ ] 2.1 给 `SidebarGrouping` 增加 `Status` 变体（唯一一处定义
      `crates/waku-client/src/persistence.rs`，保留 `sidebar_grouping_chosen` 语义），并增加
      `SidebarGroup::{NeedsYou, Running, Recent, Archived}` 四个稳定身份；验证：偏好序列化往返
      测试（旧值仍解码）+ 折叠状态按身份跨视图保留的测试。
- [ ] 2.2 扩展 `sidebar_rows_cached` 的指纹：把 `status`（状态视图下）、归档标记、归档可见
      开关、当前视图混入（只做整型混合，不得引入字符串比较，也不得把 objective 文本纳入）；
      验证：翻一个任务的状态会重新分区、归档一个任务会让它的行消失的单元测试。
- [ ] 2.3 实现状态视图的行构建：三档分档（`Waiting`+`Failed` / `Connecting`+`Working`+
      `Background` / `Idle` 且已开始）、空档不渲染、`需要你` 按 `blocked_since` 升序（时间戳最早
      = 阻塞最久在前，与 spec 的场景一致；缺该字段时按 `last_reply_at` 升序兜底）、后两档按
      `last_reply_at` 降序、扁平且不套用项目窗口与
      批量露出；验证：行快照单元测试断言成员与顺序，含"重启后顺序不变"与"改名不改变顺序"。
- [ ] 2.4 增加归档档位与"显示归档"开关（持久化），并保证当前活动任务即使已归档也仍然渲染在
      归档档位里；验证：单元测试覆盖"默认不显示"、"打开开关后落在尾随档位"、"活动任务不被
      隐藏"三种情形。
- [ ] 2.5 增加行事实缓存：一个"会话 id → { objective, 原因, 轮数, 改动数 }"的缓存，在会话数据
      变化时刷新，行构建器只读它、不得扫描 `transcript_blocks`；同时让项目分支缓存不再只在
      项目视图下解析（卡片在任何视图里都要能拿到已知分支）；验证：单元测试断言行构建不触碰
      transcript，并用 `include_str!("sidebar.rs")` 的正文断言守住该路径。
- [ ] 2.6 实现第二行的"现在"规则与项目名解析：阻塞原因 → 忙且有 plan step → 忙且有 objective →
      保持今天的内容；状态视图在第二行标出项目名，并按项目视图的同一约定解析（无项目 / 项目
      缺失）；跨项目不显示任务并不在的分支；验证：单元测试覆盖六种状态 × 有/无 objective 的
      组合，并断言闲置行与此前渲染完全一致。
- [ ] 2.7 区块头与文案：状态区块带标签与计数、可折叠、无"新建任务"按钮，状态视图下不提供
      排序控件；`locales/{app,zh-CN,ja}.yml` 增加三档与归档档位文案，归档动作文案放在第 5 组；
      验证：`bun test apps/web/src/lib/i18n-core.test.ts` 断言三份键集合一致。

## 3. Objective 生成

- [ ] 3.1 在 daemon 增加调度：转发线程识别 turn 结算 → 静默门 15s（期间又收到用户消息则放弃）
      → 每任务每小时最多 6 次 → 60s 超时 → 失败静默保留旧值；不做历史回填，resume/打开不触发；
      验证：注入假生成器与假时钟的单元测试覆盖"连续四次结算只生成一次"、"工具活动不触发"、
      "已有历史不产生任何生成"、"超时保留旧值且不报错"。
- [ ] 3.2 随包发一个 Pi 扩展（`resources/pi-extensions/`，走现有 `--extension` 通道，参照
      `resources/computer-use/pi-extension.ts`）：注册 `waku:digest` 命令，handler 用
      `ctx.sessionManager.getBranch()` 裁有界输入、用 `ctx.modelRegistry` 选模型（便宜档优先、
      `hasConfiguredAuth()` 过滤、否则会话模型）并 `complete(...)`（带超时），再
      `pi.appendEntry("waku:digest", { v: 1, text, usage, model })`；验证：扩展单测（模型选择与
      输入裁剪），并在 dev 里对一条真实 Pi 任务确认生成成功、任务的 transcript 与上下文占用不变、
      命令响应是 `disposition: handled`、provider 的会话目录里没多出文件。
- [ ] 3.2b driver 处理 `entry_appended`：过滤自定义类型与版本，交给同一套解析函数；验证：单元
      测试覆盖“收到合法条目则写入”、“版本未知或畸形则忽略并保留旧值”、“其它扩展的条目不影响”。
- [ ] 3.3 实现解析与质量规则：解析固定 schema 的输出、清理引号与 markdown、截断到行宽预算；
      拒绝含路径分隔符/文件扩展名/符号样式 token 的 objective；与已存 objective 显著重复时保留
      旧文本；与标题重复时不予存储；验证：单元测试覆盖"恶意输出被拒并保留上一版"、"同义改写不
      替换"、"真实改方向则替换"、"与标题重复被拒"。
- [ ] 3.4 在 daemon 侧解析优先级并只写解析结果：有 `thread_goal.objective` 时它就是
      objective，goal 被清空时回落到生成的摘要或空；验证：单元测试覆盖"用户 goal 胜过摘要"与
      "goal 清空后回落"，并断言客户端渲染不需要读到 `thread_goal`。
- [ ] 3.5 让 daemon 能发布 catalog revision：结算线程在生成完成后推一次 revision，值不变则不推，
      且更新 objective 不得改动 `updated_at`；验证：单元测试断言"同一 objective 不产生 revision"、
      "新 objective 产生一次且列表顺序不变"。
- [ ] 3.6 更新文档：在 `docs/titles.md` 补一节说明标题与 objective 的分工（标题仍归
      provider/用户，objective 只补目标且永不改写标题）、Pi 旁路调用的形状与失败降级；验证：
      文档中的命令与代码中的实现一致（同一处测试断言触发命令与条目 schema，文档引用该断言）。

## 4. 行工具提示

- [ ] 4.1 实现卡片：一个纯函数构建的紧凑单卡片（标题、objective、阻塞原因或 step、项目、已知
      分支、事实行），无分隔栏、无固定列宽、事实右对齐塞进短行留下的空位，签名不接收 daemon
      client / store / path；在 `src/ui/tooltip.rs` 的模块文档里说明文本提示与卡片的分工；
      验证：单元测试覆盖"字段缺失时的降级渲染"，并对内容来源做正文断言（不得读取 transcript）。
- [ ] 4.2 让键盘焦点显示同一张卡片：每行新增第二个 `ContextMenuHandle`（`session-tooltip-{id}`）
      配 `popover()`，在 focus/blur 上开关，不复用上下文菜单句柄；验证：Tab 走查——每一行聚焦
      都能看到同一内容，且打开上下文菜单时卡片正确关闭、不留半开状态。
- [ ] 4.3 守住"不发起请求"：按仓库既有做法（`command_palette`/`right_panel` 的
      `include_str!` 正文断言）加一条回归测试，断言悬停与聚焦路径不调用任何 daemon 请求；
      验证：该测试通过，并在长列表上快速划过时目视确认无卡顿。

## 5. 归档（层 1）

- [ ] 5.1 行上下文菜单增加归档/取消归档（与今天的重命名/移除并列），并对已归档任务显示反向
      动作；文案描述为"收起来"，不得暗示释放空间；`locales/{app,zh-CN,ja}.yml` 增加对应文案；
      验证：菜单项的单元测试或正文断言 + 三份 locale 键集合一致测试。
- [ ] 5.2 检索仍可达：归档任务必须留在搜索/命令面板读取的 catalog 里，并且从搜索结果打开它时
      列表仍显示这一行（当前活动任务不因过滤而消失）；验证：命令面板测试断言归档任务出现在
      结果中，加一条"归档的活动任务仍在列表中渲染"的测试。
- [ ] 5.3 归档不删任何东西：验证：归档后确认 messages、session_details、checkpoint 引用与
      worktree 目录均未改变（沿用现有 `blob_sweep` 相关的测试手法）。

## 6. 集成验证

- [ ] 6.1 端到端：在 `waku-sidebar-nav` worktree 里跑 `bun ./scripts/dev.ts`，用一条真实 Pi 任务
      走完"提交 → 结算 → objective 出现 → 状态视图显示 → 卡片显示原因/事实 → 归档 → 恢复"，
      确认没有 provider 错误、任务 transcript 里没有生成请求、任务上下文占用没有因此增长。
- [ ] 6.2 性能：构造 200 条任务的列表，在状态视图里连续滚动，按 `docs/performance.md` 的计数器
      playbook 记录行构建成本（FPS 面板为 ⌘⌥⇧F，且它按设计把窗口钉在刷新率上，不能用来测流式
      节奏）；验证：记录一次前后对比，确认没有新增 I/O 与整表重建。
- [ ] 6.3 AGENTS.md 无障碍走查：只按键盘完成"切换视图 → 定位一条需要处理的任务 → 看到卡片 →
      归档它"，确认焦点始终可见、`enter`/`space`/方向键可用、reduced-motion 下 spinner 静止。
- [ ] 6.4 `openspec validate add-sidebar-task-triage --strict` 保持通过。

## Workflow follow-up

- 本变更在 `waku-sidebar-nav` worktree 实现；在那里跑 dev watcher 会接管 `Doki Debug.app`，
  验证前需要停掉主目录的 watcher，避免两个 watcher 互相 `pkill`。
- 评审通过后按仓库约定归档该 change；`docs/titles.md` 的更新随变更一起落地。
