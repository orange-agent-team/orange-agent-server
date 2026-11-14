# 保留原件管理宿主接口

本增量提供 Runtime 接口，尚未接入桌面展示、选择器或 FRB。它沿用 schema17，不新增数据库表，也不赋予模型新的工具。归档仅改变准入预留的记账分类，文件仍在原目录、原 inode 上；它不是删除、导出、备份、压缩或磁盘配额。

## 发现和丢失回复核对

`agent_retention_patch_ids(owner, run, before, limit)` 返回属于该用户、该 Run 的已成功发布或已独立解决的 unknown 编辑操作 ID，包括已经归档的记录。按操作 ID 字典序倒序分页，before 为严格上界，limit 为1—100。不返回路径、文件内容或摘要，也不恢复普通工作区权限。未解决的 unknown 不进入此索引，仍从恢复入口处理。

`agent_retention_review_id(owner, run, operation)` 校验拥有关系及已存记录，只返回已提交归档的 review ID，供批准回复丢失时核对。None 只表示没有已验证的归档记录，不授权自动重试。拥有者可以在关闭文件选择后查询；不存在的 Run、跨用户或跨 Run 操作被拒绝。

## 独立选择能力

可信宿主在用户明确选择原工作区以管理保留文件后调用 `authorize_patch_retention(owner, run, operation, root)`。返回的 `PatchRetentionGrant` 不可序列化，不暴露 WorkspaceGrant，不能用来继续 Agent、编辑或恢复写入。能力绑定用户、Run、操作、策略、工作区物理身份和当前 Host epoch；排除全部私有根。成功编辑及经独立解决的 unknown 可以授权，未解决的 unknown 不可授权。

`request_revoke()` 立即阻止新的访问及已有 receipt 的展示；宿主随后等待 `wait_drained()`，再释放原生目录权限。Host 重开必须重新选择，旧能力与旧预览不得复用。独立能力不会恢复已撤销的普通 Run 访问。

| 接口 | 行为与边界 |
|---|---|
| `inspect_selected_retention(grant)` | 返回目标、暂存和原件的实际受限观察；持有 receipt 到投影完成，展示前再次检查 snapshot |
| `selected_retention_usage(grant)` | 返回物理工作区的活动预留记录/字节及该用户的归档预留记录/字节和限制；不是实际磁盘使用量 |
| `prepare_selected_retention(grant, request_key)` | 为该操作生成限时、精确摘要预览；不改变归档记账 |
| `approve_selected_retention(grant, review_id, digest)` | 可信 UI 明确确认后提交；绑定同一操作、当前选择和精确摘要，复查原件；事务一次性转移记账 |
| `inspect_selected_archive(grant)` | 返回已归档记录与当前原件观察；标记归档后发生的变化，仍受撤权和排空限制 |

已有归档上限为每拥有者256MiB预留及1000条记录；每个受限常规文件按固定上界预留，不根据可能继续被外部旧描述符修改的当前长度放宽准入。底层事务和事件原子提交。归档原件仍可能被其他程序修改，因此“归档”不能显示为不可变备份。到达上限必须明确拒绝，不能自动删除文件腾出额度。

## 宿主接线要求

1. 使用独立用途的 native selection token；不得接受普通读取、新建、编辑或恢复写 token 替代。
2. 持久化与授权由单 Journal 拥有线程处理；撤权信号不等待该线程。迟到选择/结果不恢复已关闭页面或能力。
3. 页面明确说明原地保留、预留额度与实际磁盘用量的区别。先显示当前原件观察和预览，再允许一次明确确认。
4. 批准返回不确定或丢失时，只查询已提交 review ID；不得把错误显示为成功或自动重发。撤权后的响应可被拒绝，即使数据库已经提交，故需要此查询。
5. 原操作若为 unknown，归档后仍为 unknown；旧 Run 不继续。数据导出和删除需要独立方案、精确批准及验证，本接口不实现这两项。

## 已验证和未验证

真实临时文件与 SQLite 测试覆盖：连续两次编辑后的保留管理、错 owner/Run/操作/root、严格分页、新选择不能复活旧预览、撤权与 receipt 排空、重开 epoch、精确摘要确认、幂等记账、归档后重开查看；另覆盖真实回执失败产生 unknown 后先拒绝归档、独立解决后才允许归档且原状态不变。模型为显式协议替身。

195项 Runtime 测试与严格 Clippy 通过，日志位于执行台账 `retention-selection-full-runtime.log`。桌面接线、实际系统选择器、导出/删除和其他目标系统均不由这些测试认证。
