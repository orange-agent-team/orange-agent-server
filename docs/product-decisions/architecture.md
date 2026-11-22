# 架构决策索引

| ID | 决策、理由与影响 | 重新评估条件 | 对应实现 |
|---|---|---|---|
| control-plane | 核心业务采用模块化控制面。将预算冻结、Job 身份、原报价与持久事件放在同一数据库事务中，便于在丢失响应和并发提交后核对。模块可单独测试；数据库升级、锁顺序和故障域需要统一管理。 | 经容量观测确认某模块需要独立扩展，且已明确事务所有者、消息幂等键和故障恢复边界 | [控制面](../../services/control-plane/src/store.rs) · [迁移](../../services/control-plane/migrations) |
| registration | 产品路线保留内部验证后的公开注册，降低消费者与供给者的接入门槛。获取账户、通过 Worker 资格审核、取得某个团队的预算和获得运营权限是独立步骤。 | 上线地区、滥用处置能力或身份渠道变化时，重新验证账户、节点和管理面隔离 | [团队授权](../../services/control-plane/src/team.rs) · [用户契约](../../crates/protocol/src/user.rs) |
| credits | 使用整数微积分及购买、赠予、贡献分桶。最高预留约束单次风险；结算关联原调用与真实计量。待入账收益须完成规定的核对后才可使用，退款跟随原购买来源。代价是对账、冲正和历史版本迁移需保留不可变业务身份。 | 增加可消费来源、改变价格维度或退款路径时，需验证并发、原分桶恢复和双人审批 | [金额类型](../../crates/domain/src/lib.rs) · [可信报价](../../services/control-plane/src/artifacts/pricing.rs) · [分账迁移](../../services/control-plane/migrations) |
| engines | E03 管理权限、模型调用、工具提交和恢复。llama.cpp 等后端通过锁定接口提供推理，受管 Python 后端提供对应媒体处理。平台保留自己的租约、审核与账本，借此在更换模型后端时继续核对原效果。 | 新后端只有在身份、工具协议、取消、完整结果及实际用量通过验收后才能进入相应支持行 | [运行时](../../crates/agent-runtime/src/engine.rs) · [本地适配器](../../crates/model-runtime/src/llama.rs) · [受管媒体后端](../../backends/pyproject.toml) |

当前无人值守开发只使用已授权的本地资源与开发积分。公开注册和商户渠道的产品设计，不代表已经具备经营、发布或真实收费资格。
