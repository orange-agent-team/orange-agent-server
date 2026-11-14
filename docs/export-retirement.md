# 导出文件移入保留区（schema20）

这是可恢复的名称移出，不是永久删除、系统回收站、空间回收或完整数据退出。仅接纳内容匹配已批准导出包的普通目标或暂存文件。冲突文件、链接、不完整包、无原始写入者绑定的旧导出一律拒绝。

## 授权与调用

调用方先独立选择目的目录获取 ExportInspectionGrant，再以独立写选择获取 ExportRetirementGrant。prepare_export_retirement(read, write, Target或Staged, request_key) 返回不可序列化的 PreparedExportRetirement，其 preview 包含精确对象观察、摘要、导出ID和五分钟有效期。approve_export_retirement 必须提交原摘要，使用多线程 Tokio runtime。证明持续持有原导出写入者锁和读写租约；关闭应先丢弃缓存准备对象，再撤权并排空。不要持有准备对象跨用户无期限会话。

派发状态先事务持久化，再创建0700保留目录和0600身份标记，持久化目录inode和随机绑定，最后不覆盖地移入item。检查前后对象身份和内容，成功记录 retained_in_capsule。失败保留对象并记录unknown；重启将残留dispatching转unknown，永不自动重放。收据失败也不得移回、擦除或伪造成功。

export_retirement_outcome / export_retirement_outcomes 提供所有者绑定的元数据恢复和有界游标列表。inspect_export_retirement 在独立读授权下核对保留目录身份、标记、权限及item实际状态。capsule_bound=false意味着没有持久绑定，不能解释为从未发生效果。匹配检查不会把unknown改成retained。

## 边界

原写入者锁仅覆盖本应用登记的导出线程，不能排除外部进程。命名空间被外部进程在最后一步替换时，捕获的替代对象也保留，返回unknown，原对象可能位于外部移动的位置。没有全局对象CAS承诺。外部旧文件句柄也可能继续改变保留对象。保留目录不可作为普通模型工具根目录，但这不是操作系统隔离其他进程的保证。

本阶段没有永久擦除、保留区恢复发布、预算释放、未知文件强制清理、自动修复原导出成功状态或跨OS认证。桌面须独立接入选择、预览、批准、撤权和结果核对；不得仅导入新Runtime就标为端到端完成。

## 验证

实际临时文件/SQLite测试覆盖成功捕获、错误摘要、撤权、目标变化、保留目录冲突、回执提交失败、末刻替代对象、暂存包与冲突目标共存、重启和无重放、所有者隔离、有界查询，以及目录同内容不同inode、身份标记篡改/链接、权限放宽。全部保留原数据。schema19固定旧CLI真实数据升级20并保留原批准，旧CLI拒绝打开且不修改新数据库。

统一执行证据：export-retirement-runtime-full.log（207项，当时未包含最后一项绑定测试）；export-retirement-binding-tests.log（4项回收测试/严格Clippy，最终总数208）；export-retirement-schema19-compatibility.log。共享固定源导出另运行完整测试并记录摘要。
