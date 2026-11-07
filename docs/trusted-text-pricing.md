# 服务端可信文本计量与开发积分结算

后继更新：[CPU清理生命周期](cpu-cleanup-lifecycle.md)已接审核CPU的云回执、attempt关闭、实际再次调度、未交付零费资格和终态pin释放。以下保留计量切片起点；GPU/生产审批/实际支付等边界不变。

本切片用服务端拥有的固定模型进程计算使用量，通过真实报价API、ResponseCommit及原子开发账本完成闭环。没有外部支付、提现、真实资金操作或生产价格发布；云端资源cleanup仍是独立未完成事实。

## 计量所有者与版本

E04 `TrustedMeter`启动并持有实际ManagedCpuModel、私有socket/API key和模型文件锁。客户端或公共Worker不能提供计量URL、计数或构造MeasuredInput/MeasuredOutput。计量队列最多8项，串行拥有模型；请求有10秒单调期限，取消/故障不伪造零使用量。服务关闭等待实际模型退出，失败不宣称资源释放。

输入通过固定后端的实际 `input_tokens` 模板投影接口计算；输出按DR-01 `orange-visible-components-v1` 对文本、工具名和JCS参数逐分量完整分词求和。协议ID、隐藏字段、native_usage和分片边界不成为收费单位。规范输出上限与原生生成上限分别检查。

MeterDescriptor摘要绑定模型SHA、可执行文件SHA、模型版本、上下文/执行配置、输入规则、输出规则及工具schema校验规则。当前只支持实际验证的本机llama CPU组合；其他协议/模型/OS组合不得套用此计量证明。

工具参数采用锁定jsonschema0.56.0（MIT，要求Rust1.85，当前Rust1.97.1已兼容编译）。关闭默认HTTP/文件解析特性，使用offline模式、Draft2020-12与线性正则；不接受远端/文件引用、动态/递归引用、未知格式或无界schema。支持有界、无环的本地JSON Pointer引用。schema最多64KiB/4096节点/32层，参数最多64KiB/4096节点，并限制展开校验工作量；不符合规则的组合明确拒绝，而非跳过校验。工具必须来自原请求，参数满足schema，不能复用历史工具调用ID。依据：[官方选项文档](https://docs.rs/jsonschema/0.56.0/jsonschema/struct.ValidationOptions.html)、[官方功能说明](https://docs.rs/jsonschema/0.56.0/jsonschema/)。实际特性以Cargo.lock和cargo tree为准，未启用resolve-http/resolve-file。

## 开发价格准入与报价

PG0024增加trusted_meter_profiles、不可变text_prices、quote_metering、job_usage_facts和job_settlements。text_prices只能标记development_only；当前没有公开价格修改接口，也没有将开发自动化替代生产Admin MFA/双人批准。测试中的模型/价格准入由明确夹具建立，生产价格治理仍需后续实现。

`GET /api/v1/text-prices`需要真实User会话，列出匹配当前服务端计量配置且在线的已准入offer。`POST /api/v1/owners/{owner}/text-quotes`要求当前用户、Origin/CSRF、Idempotency-Key、预算提交授权和精确输入内容权限。请求只能选择price_version、输入upload/version、root budget与规范输出上限，不能自填单价或单位。

报价先实际读取输入、调用可信计数器，再在事务内复核当前会话、ACL版本、预算、在线节点、审核profile与价格准入。Quote绑定原投影、输入版本、实际输入单位、规范输出上限、整数价格、费用比例和原供应方owner。相同请求键返回原快照；冲突不能改价。默认报价有效60秒，Job deadline5分钟；报价本身不冻结资金，提交Job才使用既有原子冻结路径。

撤销价格/计量准入后，不接受新报价、新冻结或新派发。已经合法交付并记录的事实仍按原不可变快照结算，不能升级价格后偷偷重算旧Job。

## 发布前事实与原子账本

有quote_metering的Job必须使用匹配的服务端计量进程重新检查输入/输出，规范输出超限、工具参数非法、投影/模型/profile不匹配时不发布、不产生usage fact、不结算。可信使用量与ResponseCommit在同一个事务建立，因此后续结算不需要重新读取被删除/撤权的正文。

交付后，内部结算器使用原Quote与不可变usage fact计算费用。它锁定预算、按稳定owner顺序锁钱包、再锁Job，原子完成：扣除本Job的冻结桶、退回未使用额度、更新预算spent/reserved、增加供应方pending_earned、记平台费用、平衡账本、Job settled及outbox。供应方收入只是开发积分pending，不是可提现到账。

business_key为settle:{job_id}且唯一。重复提交和并发恢复不得多扣；事务失败保留已交付结果与pending结算，不能留下半笔扣款/分成。内部维护只重试已经提交且有可信usage fact的Job。历史query仍只返回原事实，不直接发起结算。旧的未计量开发夹具不会被猜测补账。

## 进程配置与验证

User API可指定私有 `ORANGE_METER_CONFIG`，模板为config/trusted-meter.example.json。Counter目录须独立、私有并已存在；配置中的模型/后端必须通过实际SHA验证。User API无论正常退出或启动后失败，都会等待Counter停止；对象I/O排空与Counter关闭顺序独立明确。

`python3 scripts/check-trusted-pricing.py`使用现有完整固定Qwen和实际计数接口，覆盖伪造native_usage不影响费用、规范输出上限、价格撤销、非法工具参数、账本故障全回滚、并发恢复一次结算，以及实际Worker/User API/独立服务端Counter/HTTP报价与结果读取的完整开发闭环。价格/身份/enrollment/初始准入仍为夹具；实际进程、字节、模型计量及开发账本是真实执行。

未完成范围：生产价格发布审批、商户/充值/提现、更多收费组合、取消持久文本与失败/被替换attempt的完整终结、云端cleanup/资源再次调度、最终pin释放、完整前端计费明细及跨OS/GPU认证。不能将Job settled当GPU回收证明，也不能把pending_earned称为实际到账。
