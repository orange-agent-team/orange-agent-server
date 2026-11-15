# 来源感知记忆管理与同一任务更新历史（schema25候选）

本切片为共享 Rust Runtime 增加管理读取/分页，以及同一 Run 对既有记忆进行批准更新后的继续执行能力。没有完成桌面管理界面、模型发现模式或全部记忆产品；主线与桌面的固定 Runtime 引用保持原版本。

## 管理接口及信任边界

`MemoryManagementAuthority::new(&MemoryGrant)` 使用 Host 已认证、绑定 actor/policy/会话/scope 修订的能力，不要求虚构工作区或 Agent Run。派生记忆需要通过 `with_source_workspaces` / `with_source_memories` 显式补齐原始来源权限，不能由正文或模型声明权限。

`Journal::read_memory_verified(authority, MemoryReadSelection)` 接受条目 ID、可选预期版本、起始字节、最大字节以及管理专用的 include_disabled。返回的 `VerifiedMemoryRead::view(&Journal)` 再次核对当前授权、版本和来源 lease。读取遵守已有 1–4096 字节及 UTF-8 边界规则。include_disabled 只允许显示被明确选择的根条目；禁用的依赖仍被拒绝。整个 scope 被禁用后不能取得现有活动 MemoryGrant，本切片未提供禁用 scope 的专用管理授权。

`Journal::list_memory_verified(authority, cursor, limit, include_disabled)` 返回持有证明的 `VerifiedMemoryPage`。limit 为 1–100，每次最多检查 200 个候选（仅查询 201 个 ID/版本用于判定后续页），整个调用有 10 秒期限。先校验来源，后投影标题。权限或来源不可用的条目被过滤，仅报告 omitted_unavailable；不返回其 ID、标题、正文或精确数量。损坏的结构和数据库错误仍返回错误。scan_limited 表示达到扫描上限且存在后续候选。

next_cursor 是随机 UUID，真实位置只保存在当前 Journal 内存中，绑定 actor、scope、能力环境及 include_disabled。有效期 5 分钟，最多保存 256 个未过期游标；超过容量明确拒绝。关闭或重新打开 Journal 后游标失效。游标不是授权：每页重新验证当前能力与来源。页面不是冻结数据库快照，期间修改会在下一次读取重新判定。

来源证明持有不含来源正文的权限 lease。工作区 ToolReceipt 转换时丢弃正文和模型文本；记忆来源凭据清除标题/正文后保留版本、授权与排空计数。管理返回值自身仍包含调用者获准看到的内容。释放对象不等于安全擦除内存、SQLite WAL、备份或外部副本。

## 自身更新历史

Run 原始模型输入和工具结果保持不变。重新使用旧版本时，必须验证旧版本到当前版本之间每个替换均来自相同 owner/Run 的真实已批准草稿、固定工具意图和相应操作绑定。每一跳必须是精确 expected_version 的启用 Put，版本连续，不接受外部替换、删除、禁用或缺失证据。最多检查 100 个版本跨度；已有来源图的 256 节点/16 层限制仍生效。

预览、批准、执行和恢复都验证当前事实来源，并继续检查原始不可变来源图。不能把“后来更新了内容”视为删除其输入依赖的证明。当前版本 lease 覆盖实际效果生命周期；同一事务对目标条目的精确更新仍使用既有 check_after 检查。

丢失结果回执不会触发自动重写。Host 使用实际草稿提交证据执行 `ConfirmAppliedAndContinue`，更新操作回执并应用上下文；反复确认会拒绝。未知状态下再次执行会撤回旧工作区能力，必须取得新的明确授权才能恢复。终止或拒绝后的内容为空的 recovery_required 元数据仍可读取，这不恢复正文权限。

## 格式与兼容

SQLite 仍为 schema25/source-lifetime.v1：没有新增持久表、字段或工具协议，游标完全属于进程内会话。现有 schema25 读取器仍解析相同格式；此前读取器可能拒绝自身多次更新后的继续执行。模型 memory_read.v1 仍要求明确 entry_id，不包含列表或发现能力；后续显式v2入口见 [memory-discovery-v2.md](memory-discovery-v2.md)。客户端仍必须消费提交固定的源包，不能隐式依赖相邻未提交源码。

## 验证与限制

新增测试使用实际 SQLite 与临时目录、显式脚本模型输送器，不能当作真实 LLM 认证。覆盖管理读取无虚构 Run、跨 actor/筛选条件/会话游标拒绝、禁用根条目、来源撤回后旧凭据失效、201 个故障注入记录下的扫描限制和不泄露分页；同一 Run 连续更新、外部替换/删除/禁用阻断、写回执丢失后的显式恢复、自己禁用后的正文阻断，以及更新后保留传递文件依赖。

来源文件在 API 执行时重新读取并验证快照，view 检查能力和数据库版本；这不是文件系统对任意外部写者的排他锁。再次使用时应重新调用来源感知接口，不能把复制出的字符串当作永久授权凭据。桌面 FRB/UI、真实模型、跨端验收、禁用 scope 管理、模型发现和知识/连接器来源仍须后续实现。
