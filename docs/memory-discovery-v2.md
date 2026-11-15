# memory_read.v2 发现与分页（schema27候选）

该切片接入实际 Rust/SQLite 的记忆发现执行器。Host 显式使用 `Journal::start_agent_discovering` 启用；后续仍通过 `advance_agent_authorized`、`approve_agent_memory` 及当前授权恢复。工具可见名称仍为 memory_read，共用现有工具名额。

## 固定协议及兼容

schema27 新增 runtime_memory_protocol，记录每个 Run 的 read_version=1或2，禁止修改/删除。迁移已存在Run为1，既有start_agent/start_agent_authorized保持1。v2启动的配置摘要包含明确协议选择；同run重新注册不能在1与2间切换。`agent_memory_read_version(owner,run)`返回Host恢复需要的持久版本，不授予scope权限。

v1参数、序列化、摘要和pins保持原值。v2使用独立执行pin `memory_read.v2`，保存在contracts/fixtures/memory-tool-pins.v2.json；memory_write.v1及文件/问答工具pins保持原值。模型参数、持久目录快照、批准pin和恢复目录均按同一个Run版本选择。内部持久请求的tool标签为memory_read_v2，模型仍只能调用memory_read。

## 请求与结果

条目模式：`{"mode":"entry","scope_id":"personal","entry_id":"note","expected_version":"1","start_byte":0,"max_bytes":4096}`。预期版本和字节参数可省略；默认从0开始，最多4096字节。结果为 `{"kind":"memory_read","data":{"mode":"entry","value":MemoryValue}}`。

发现模式：`{"mode":"list","scope_id":"personal","limit":20,"cursor":null}`。limit可省略，默认20，允许1–100；cursor来自上一页且可省略。结果为 `{"kind":"memory_read","data":{"mode":"list","page":ManagedMemoryPage}}`。page含scope_id、entries、next_cursor、omitted_unavailable、scan_limited。entry摘要含entry_id、version、title、enabled=true、origin、content_bytes，不包含正文。

模式字段不可混搭；禁止actor、policy、sourceRun、include_disabled、未知字段或超范围limit。scope枚举来自该Run实际记忆授权；证明来源用的额外Grant不进入模型可见scope。只读scope不暴露写入能力。

## 来源、批准与提交

entry/list都需要对精确参数摘要批准。读取前验证固定意图、授权和已使用上下文；实际列表调用来源感知管理扫描，根条目仅active，文件/记忆祖先必须具有真实当前来源权限。过滤的记录不返回ID、标题、正文或精确数量。

每页最多扫描200候选、返回100条，扫描有10秒期限。每项前后显式检查期限，避免仅依赖Future timeout忽略同步工作耗时；有界扫描不是OS级硬实时抢占。读取结果持有来源lease至提交，事务结束前再次检查当前权限、结果投影、批准期限、Run活动状态、取消和deadline。迟到撤权或批准失效回滚全部结果和引用；已claim但未确认的读取继续按未知操作门槛处理，不产生伪造成功结果。

runtime_memory_sources保存每个返回条目的精确scope/id/version；空页和受控读取失败没有引用。checked_memory_source_rows按已提交v2结果完整匹配引用集合；构建后续派生记忆时遍历所有摘要引用。后续模型请求前逐条重验版本/状态/来源；删除、禁用、来源撤权及额外/缺失引用都会阻断旧内容。来源校验按操作复用同一固定目录和已提交结果校验，仍逐条校验每个条目。

同一Run的已批准连续更新沿用既有历史规则，旧模型输入保持原样，只有真实连续启用Put及完整来源图才允许继续。列表不保证以后新增条目仍不存在，也不声明整个scope的冻结快照。

## 游标及恢复

游标是当前Journal会话内随机UUID，绑定actor/scope/能力环境/筛选模式，五分钟失效，最多保留256个活跃游标。重启保留Run协议和已提交来源，旧游标失效。未知/过期游标在当前权限仍有效时生成有界memory_unavailable失败，不伪造空成功页；Host/模型可以提出新的首屏请求并重新获得批准。被撤回的scope不能通过失败结果或游标恢复权限。

来源验证可使用主scope加最多23个额外记忆Grant、主工作区加来源工作区合计最多17个；这些是验证材料，不是新的可调用scope。保留底层来源图节点、深度、字节和配额限制。

## 验证与后续

测试使用真实SQLite、实际临时文件和明确脚本模型输送器。覆盖两页摘要→条目读取→派生写入→完成、全部来源保留、100条页及101条跨页、空页、重启/过期游标、只用于证明的scope不出现在参数中、缺文件权限的过滤、来源撤权、删除/缺引用阻断、v1与v2恢复、精确固定pins，以及提交前批准/scope失效的原子回滚。旧schema26固定CLI创建的数据通过单独升级检查。另由check-memory-compatibility.py把同一最小测试宿主分别链接固定旧Runtime和当前Runtime，实际旧程序创建/批准/读取记忆，再由新程序恢复原始工具内容；核对结果、来源、目录快照完全保留，协议仍为v1且无重放，旧程序非修改式拒绝新库。测试宿主只使用已有锁定依赖子集。

真实本地LLM的memory交互、桌面FRB/界面和固定包消费仍需完成；全scope禁用后的专用管理授权、知识/连接器来源也在后续范围。本候选不代表整个记忆功能或G0—G6生产应用已交付。
