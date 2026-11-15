当前来源图实现已推进schema24，已替代下文schema23的粗过滤；现状和剩余工作以[memory-provenance.md](memory-provenance.md)为准。下面保留schema23接口与阶段历史。

# 记忆工具执行候选（schema23，未完成/不可合并）

本候选把MemoryStore接入受控Agent循环。它尚不是TK-16/17的完整交付：文件/知识来源的持续ACL谱系、混合文件写入的来源生命周期、记忆发现/列表工具模式、真实模型与管理UI仍需完成。现有临时拒绝条件必须由完整实现替代，不能据此缩小原计划范围。

## 已实现的执行边界

`AgentRequest`区分Workspace和Memory请求，旧文件请求的JSON及摘要保持相同；WorkspaceGrant不能执行MemoryRequest。Memory参数禁止actor、policy、source_run和origin字段；新条目ID由Host在模型响应提交事务中分配并持久绑定到Operation。来源Run和ModelInference分类由执行器指定。

`AgentAuthority::read(...).with_memory(...)`使用真实MemoryGrant，最多8个不重复scope。Run保存scope/修订/读写模式及独立环境摘要，恢复时需要新Journal实例的当前授权；可序列化的MemoryCapability只是元数据，不能充当Grant。未选scope的Run不暴露memory工具。模型请求中的scope_id枚举只列出实际选中的范围；memory_write仅列可写scope。基础工具执行pin固定于memory-tool-pins.v1.json，Invocation进一步保存具体scope的工具快照。

模型响应完整验证并提交后才创建操作意图和条目绑定。memory_read也需要独立批准；memory_write先产生真实草稿，显示内容、来源、版本、分类和摘要，用户确认后才可执行。`approve_agent_tool`及普通Store批准均拒绝绕过Runtime记忆操作的专用批准。

写入事务内同时检查当前Scope权限、原Operation批准、Run期限、取消信号和文件读会话状态。事实和MemoryDraft回执原子提交；随后保存工具结果和上下文。后者失败时保留unknown，不重放事实写入。只读缺失/版本不可用在权限仍有效时返回有界错误，不伪装成成功或数据写入。

## Host接口

| 接口 | 目的 |
| --- | --- |
| start_agent_authorized / advance_agent_authorized | 使用显式组合能力启动和推进Run；默认旧入口不能隐式获得记忆权限 |
| agent_memory_requirements | 取得拥有者可见的恢复权限元数据，不返回记忆内容 |
| agent_view_with_memory | 验证当前scope及已使用条目版本后返回内容；普通agent_view对Memory Run清除pending正文、问题和final_text |
| agent_memory_preview | 从已提交意图生成/取得精确草稿预览；不会学习生效 |
| approve_agent_memory | 读取绑定参数摘要，写入绑定草稿摘要；回答问题不等同批准 |
| answer_agent_question_authorized | 在记忆权限有效且来源仍有效时接受用户答复，只改变上下文 |
| agent_memory_outcome / agent_memory_metadata | 当前权限下的结果查看，以及仅拥有者可见的历史草稿回执元数据 |
| reconcile_agent_memory | 明确确认已提交/未提交，并选择继续或结束；不得按当前内容看起来相同伪造历史成功 |

未核对的memory_write按稳定scope_id和actor隔离，不能通过新建Run或更换环境摘要绕过。核对依据是同一SQLite事务产生的已批准草稿回执或未提交状态。继续要求原有当前权限，且既有来源仍有效；结束只修改拥有者的操作/Run状态，不恢复Scope或工作区访问。旧事实后来修改/删除与历史效果确认是不同事实。期限已过不会重新调用模型。

取消Run会清理该Run仍待批准的草稿payload。权限撤回使等待中的模型调用收到适配器取消信号并丢弃回应；这不是推理进程已排空或GPU已回收的证明。

## 仍须完成，不能作为永久限制

1. `require_independent_memory_source`暂时拒绝源Run含已完成非memory工具的学习；它也会保守拒绝一些不真正依赖那些内容的情况。必须从已提交ModelInvocation实际使用的工具结果建立来源图，以当前显式授权重新验证，而不是保留这个粗略过滤器。
2. memory与create/edit组合暂时在启动时拒绝。必须绑定来源撤回、效果取消以及后台文件工作线程的实际排空，才能开放。仅检查一次scope或丢弃Future不能证明文件效果已停止。
3. 直接记忆结果会在下一次模型调用/授权展示前核对版本和权限，但文件/知识/跨记忆的传递依赖、旧messages/checkpoint/索引失效尚未完整连接。已禁用或被替换的源会使旧上下文停止，而不是静默复用。
4. 当前memory_read读取指定条目；缺少用于发现现有条目的有界列表/范围模式。新增模式需同时更新固定契约、来源引用的多条目表达和兼容策略。
5. 没有桌面FRB/管理面板或真实LLM验收，不能从模型替身测试推断实际推理质量、完整产品流程、其他OS或生产就绪。

## 证据

实际Rust/SQLite测试覆盖模型提案→精确批准→保存→读取→完成、删除后旧结果拒绝、无权限/只读/伪造字段/未授权scope零意图、运行中撤权、迟到批准失效原子回滚、当前scope重建、问答不提权、读缺失错误、unknown隔离及显式核对、无数据权限的仅元数据结束。模型输送由明确的ScriptedModel替身提供。

旧工作区序列化与执行pin保持不变。固定schema22 CLI数据升级23和旧CLI不修改式拒绝已实际验证。候选仍未合入主线，详细日志和未完项见统一台账memory-tools-staging.json。
