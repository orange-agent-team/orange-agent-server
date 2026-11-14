# 记忆存储候选（schema22，尚未接入Agent工具）

本模块实现Journal内的受控MemoryStore，使用现有私有SQLite数据库和单Host锁。主线仍为schema21；此候选未合并，不能据此把TK-16、TK-17、AG-012或记忆UI标为完成。

## 数据与权限

记忆归属明确的scope，scope有固定owner、个人/共享类型、启用状态和权限修订。个人scope不能添加其他成员；共享scope由认证owner逐一配置read/write/revoked，团队角色不自动获得权限。授权后返回不可由模型构造/序列化的MemoryGrant，绑定当前Journal实例随机标识、数据库Host epoch、scope修订、actor、policy和明确读/写模式。另一个Journal即使同名scope/actor/epoch也拒绝旧Grant。进程重开、撤权或权限修订使旧Grant和缓存读取凭据失效。request_revoke仅是即时信号；Host必须等自己的Journal串行队列屏障完成才能确认关闭。持久成员撤权在同一Journal队列中提交；不可把发出信号当作所有在途事务已排空。

记忆条目有entry_id、版本、当前状态active/disabled/deleted、用户声明或模型推断分类、来源actor/Run和payload摘要。正文、标题和启用标志放在可清理payload中。模型适配器必须由Host生成/绑定稳定对象ID和实际来源Run，并强制ModelInference；不能相信模型声称的用户声明、actor、policy或scope权限。

## 草稿与确认

- prepare_memory_write创建Put/Delete草稿，要求写Grant、当前来源Run、policy和预期版本；没有学习生效效果。
- view/review_memory_draft返回明确来源、分类、内容、预期版本、五分钟有效期及摘要。重开后须使用新Grant重新审阅，不恢复旧批准。
- approve_memory_write核对精确摘要，并在单SQLite事务内做版本CAS、更新事实、使旧版本候选失效、写入结果和审计。失败整笔回滚；回应丢失由memory_draft_outcome/by_key查询，不再应用第二次。
- discard_memory_draft只能使待批准草稿失效并清空payload；已批准事实必须走新的编辑或删除预览。列表/查询元数据不返回失效或已批准草稿正文。
- 同键异参、来源Run不属于actor、被取消的来源Run、过期预览、旧版本或权限变化均拒绝。旧待批准版本在其他编辑提交后失效并清空payload。

## 读取与删除边界

read_memory按scope/条目/可选精确版本及UTF-8边界范围读取。模型路径应固定include_disabled=false；include_disabled=true只供用户明确打开管理视图。read/list返回受保护receipt，view再次检查当前授权、条目版本和删除/禁用状态。应用必须在展示和上下文重用时再次调用view；已经交付给调用者的字符串不能被内存魔法追回。

删除保留版本化墓碑，清空当前正文、标题和相关草稿payload；已批准草稿只保留摘要/来源，不另存旧正文。删除条目的所有已登记贡献Run在同scope内进入禁止重新学习集合，同时清理这些Run的其他待批准草稿；不允许以新key从同一旧Run重新学习。共享权限撤回或scope禁用清理scope内待批准payload并使旧读取失效。

未接入模型上下文、索引或后台学习，所以不能宣称已完成这些路径的删除传播。下一步必须把Memory引用加入Invocation/Operation和checkpoint上下文组装，校验来源工作区/知识ACL，防止缓存旧tool-result绕过Store。当前也不承诺SQLite freelist/WAL、备份或系统快照的物理擦除。

## 限额

每owner最多128个scope；每scope最多64个已登记成员、1000个条目（含墓碑）、4MiB当前payload、10000条草稿元数据，最多128个待批准草稿。单payload的JSON UTF-8最多32KiB，标题最多128字节且无控制字符。读取片段1—4096字节；列表1—100条，游标显式。上述为候选默认限额，长期归档/压缩/服务端PostgreSQL适配未完成。

## 验证与后续

测试使用真实私有SQLite，覆盖两用户同名条目隔离、显式共享/撤回、单独批准、恶意内容按数据保存、同键异参/分类变化、版本竞争、UTF-8范围与游标、禁用/删除墓碑、同scope旧来源Run拒绝、数据库回执故障整笔回滚、跨Journal隔离、重启重新审阅和拒绝payload清理。最后额外的提交前撤权注入证明事实与回执整笔回滚。详细结果见统一执行台账memory-store-*日志。

接下来需要：完整工具schema/固定pin与Agent循环接入；运行上下文和来源ACL重新校验；用户查看/批准/编辑/禁用/删除界面；真实Agent/桌面集成验收；独立固定共享源码导出消费。模型辅助学习必须走现有ModelGateway预算与purpose，不新增免费调用旁路。
