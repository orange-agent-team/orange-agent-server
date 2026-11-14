# 编辑恢复宿主接口（候选增量）

## 已实现的索引

`Journal::agent_unresolved_patch_ids(owner, run, before, limit)` 只返回该拥有者、该 Run 的未解决 `apply_patch.v1` 操作 ID。必须已有发布计划且执行状态为 unknown；已解决的操作不再列出。操作 ID 按字典序倒序分页，before 为严格上界，limit 为1—100。不存在或不属于调用者的 Run 被拒绝；不返回路径、参数、内容、摘要或授权。

取消或撤销 Run 后，不确定的效果仍可能存在，所以拥有者可继续发现这些 ID。它不恢复工作区授权，不解除隔离，也不重发操作。CLI `pending-edits [before]` 在打开工作区/模型密钥之前执行，只返回 operation_ids 和 next_before；可以在目录已移走、密钥缺失时用于发现待处理记录。

实际测试使用回执失败后的真实已修改文件：重开→索引→独立恢复授权→批准保持当前文件→解决后索引清空。原操作仍保持unknown，不能伪装成原工具执行成功。另覆盖错误拥有者/Run、无效游标、页大小、取消后索引、无工作区/密钥的CLI查询与文件不重放。

## 已实现的独立恢复能力

`authorize_patch_inspection(owner, run, operation, root)` 只在独立native读取选择后使用。返回非可序列化的 `PatchInspectionGrant`，不暴露普通WorkspaceGrant，也不能作为开始Run或修改文件的权限。它使用Journal的全部私有路径排除清单，核对拥有者/Run/操作/工作区物理身份并绑定Host epoch。未知操作在撤销或取消Run后仍可经新的明确读取同意检查，原工作区权限不会被重新激活。

`inspect_selected_patch` 返回带读租约的实际快照；`request_revoke` 使后续快照访问失败，`wait_drained` 必须等receipt释放后才完成。Host重开后旧能力失效，不能沿用旧scope。

写入另经native write选择后调用 `authorize_selected_patch_recovery`，不能从读取能力转换。后续调用 `prepare_selected_patch_resolution`、`approve_selected_patch_resolution`、`dispatch_selected_patch_resolution`、`publish_selected_patch_resolution`，绑定相同操作、当前读写环境和精确快照摘要。更换read scope不能复活旧审批。派发后、发布前撤权也进入标准unknown清理，不遗留可被当作仍在运行的dispatch标记；数据库本身不可写时仍依赖重开后的保守核对，不能声称有成功回执。

已通过190项Runtime测试和严格Clippy：真实回执失败→独立检查/批准→保持现状；读取撤权及排空、重开epoch、错误root/owner/Run；实际捕获旧文件后分别恢复原件或发布暂存；晚到目标文件保持原样，恢复失败后仍保留不确定状态。模型响应是显式替身；原操作不改成succeeded，旧Run不自动继续。

## 桌面接线要求（已由桌面实现并独立验收）

1. 内容无关的Run索引可在没有文件权限时列出ID，但不能显示旧参数或文件内容。读取恢复快照必须持有当前独立读取能力。
2. 普通工作区授权在隔离状态下仍拒绝。重开后应使用明确的恢复检查选择器建立仅供检查的read scope，并对照Run/持久工作区的物理身份；不能静默绕过普通执行隔离。
3. `inspect_patch_recovery` 返回带读租约的 receipt。宿主在投影完成前持有它，撤权后不再访问 snapshot。只呈现目标、暂存、原件的存在性、摘要和匹配关系；不自动开放受保护原件路径。
4. 执行恢复另外请求write scope，调用 `authorize_selected_patch_recovery`。该能力绑定操作、随机环境及Host epoch，不能从普通读取、新建或编辑权限转换。每个native选择token有独立用途，迟到回调与释放要使用现有的先信号后排空机制。
5. 用户选择 KeepCurrentTarget、RestoreOriginalToMissingTarget 或 PublishStagedToMissingTarget 后，调用prepare得到实际 snapshot/digest；只有对应精确摘要可批准。恢复目标必须缺失的两种选择不能覆盖现有目标。
6. `approve_selected_patch_resolution`、`dispatch_selected_patch_resolution`、`publish_selected_patch_resolution` 使用单Journal拥有线程，写入期间保留独立的撤权信号通路；未知结果不得自动重试。晚到结果不得恢复已撤销展示。
7. 成功恢复只代表当前文件状态已核对/处理，旧Run仍终止，原操作仍unknown。UI不能显示“原任务已完成”或自动继续模型；新的Run必须重新走原权限与批准流程。

## 尚需验收

桌面API/FRB/native scope/UI、实际重开后授权、恢复批准及撤权/排空已有实际macOS集成证据，见执行台账DESKTOP-RECOVERY-001和retention-recovery-regression.log。保留原件管理后续候选也已接入，见retained-file-host-interface.md。模型与系统选择器回调是明确替身；人工系统选择器、其他OS、导出和删除尚未认证或实现。

## 丢失恢复回执的只读核对

`agent_patch_recovery_outcome(owner, run, operation)` 校验拥有者、Run、操作绑定和已存回执摘要，只返回原操作状态及可选的恢复决定ID/choice。没有文件路径、内容或摘要，不恢复任何文件能力。decision为空只表示没有已验证的恢复完成记录，不表示没有发生文件效果。成功处理后原操作仍unknown，旧Run仍停止。

实际测试覆盖处理前无完成记录、错误owner/Run/operation拒绝、权限撤销后查询、Journal重开后恢复同一决定，以及输出字段不包含文件数据。用于后续桌面写入批准丢失响应时核对结果，禁止据此自动重试写入。190项Runtime及严格Clippy通过（recovery-outcome-runtime.log）。
