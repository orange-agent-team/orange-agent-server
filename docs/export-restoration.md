# 保留区恢复为新副本（schema21）

此文描述Runtime能力；桌面消费端尚固定schema20，不能宣称已有恢复按钮。

恢复始终保留保留区原件。用户明确选择相同目的目录、指定合法的新数据包文件名并批准精确预览后，创建新副本；已有目标、链接和目录不会被覆盖。需要新的ExportRestorationGrant类型，不能把之前的移入保留区授权当作恢复授权。

## 调用

1. 新的ExportInspectionGrant绑定已有导出记录和实际目的目录。
2. authorize_export_restoration(read, root)取得独立写授权。
3. prepare_export_restoration(read, write, retirement_id, file_name, request_key)读取绑定保留区，核对实际数据包及空目标，取得注册写入者排他锁和读写租约。返回不可序列化准备对象，preview包含恢复ID、原导出/保留记录ID、目标名、精确源对象、摘要与五分钟期限。
4. approve_export_restoration(read, write, prepared, digest)先持久派发，再将验证的字节写入独占0600暂存文件，fsync并使用NOREPLACE发布。源保留项不会移动或删除。目标验证及收据事务完成才返回succeeded。
5. export_restoration_outcome/outcomes恢复查询元数据；inspect_export_restoration在新读授权下实际检查恢复目标与暂存。匹配文件不修复unknown，也不自动重试。

每个所有者至多1000条恢复记录，列表1—100条；文件3MiB上限、稳定字段、路径校验和旧epoch拒绝。取消/撤权会使准备对象失效；排空前必须先丢弃缓存对象。当前要求多线程Tokio宿主。

## 状态与失败

prepared→dispatching→succeeded/unknown；重开将dispatching置unknown，不重放。receipt丢失、目标晚出现、源被改动、暂存替换或撤权，均保留已有对象，不清理临时副本、不覆盖其他内容。历史导出与保留区状态不变；没有预算或空间释放。

恢复的对象是完整导出JSON数据包，不自动解包到Agent工作区，也不重新授予原工作区权限。外部文件句柄和命名空间仍可能变动；依赖前后身份/内容检查并在异常时记录unknown，不宣称排除所有外部写者或全局CAS。

## 验证

真实临时文件/SQLite测试覆盖独立授权、错误摘要、同键重取预览、成功副本与保留原件、目标冲突（批准前与最后发布前）、撤权、提交回执失败、源变更、暂存替换、重开查询、检查权限撤销和所有者隔离。固定schema20 CLI兼容检查验证旧编辑/批准数据升级21及旧二进制不修改式拒绝。

执行证据存于统一台账export-restoration-*日志；不能用Runtime测试替代桌面原生集成测试。schema20桌面已验证候选保持独立，待固定schema21源码归档后再接入消费端。
