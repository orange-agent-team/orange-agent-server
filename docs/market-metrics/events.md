# 最小事件与隐私边界

PRD-006.03。输入由授权的取数方在限定群体、窗口与截止时刻内生成。键使用具有秘密密钥的 HMAC-SHA-256；同一导出使用一致的主体命名空间，节点、槽、订单、作业及收据分别使用域分隔。正常导出可使用 Rust 的 ExportKeys 构造主体与收据的分域代号。每份成本覆盖声明另附独立收据代号。密钥留在取数环境；报表只输出计数、时间长度、金额与比例。跨期复购需要在授权账本侧准备同一统计群体必要的历史购买关系。轮换密钥后不可直接拼接两个群体。

| 事件 | 业务依据 | 减量投影 |
|---|---|---|
| node_registered / capacity | 节点登记、同意、认证、连接失效、暂停或隔离 | 节点/槽代号、有效起止和状态；无设备序列号、IP 或证书 |
| job_submitted / attempt / job_finished | 原提交、带围栏的尝试和最终交付 | 作业、付款方、尝试及节点代号，时间与胜出关系；无输入、工具参数、音频和模型回复 |
| purchase / refund | 测试或正式渠道分别核对的原收款及退款凭据 | 原订单代号、原币种、最小货币单位及对应积分 |
| chargeback / chargeback_recovery | 实际渠道扣减及恢复 | 原购买与争议代号，原币种内的金额；仅有争议告警不能替代现金凭据 |
| settlement / earning_released | 原消费分桶、平台与供应者分账、独立收益释放 | 交易代号、积分及原购买批次用量；不导出钱包详情 |
| expense / cost_coverage | 可核对的变动成本凭据及数据覆盖声明 | 币种、成本类别和金额；未知成本不填零 |

相同收据的相同事件幂等；同键异载荷、同业务重复结算、未交付收费、跨付款方批次、重复释放、越界退款均拒绝。拒绝未知字段。输出只报告错误位置或固定错误类别。支付、作业及成本来源须经授权，格式有效本身不证明采集许可、输入完整性或真实收款。

[版本化 JSON Schema](../../contracts/json-schema/market-metrics.snapshot.v1.json) · [实际收入来源视图](../../services/control-plane/migrations/0064_merchant_statements.sql) · [购买批次](../../services/control-plane/migrations/0059_purchase_lots.sql) · [供应者释放](../../services/control-plane/migrations/0066_provider_earned_credits.sql)
