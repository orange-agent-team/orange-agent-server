# 算力市场指标与对账

PRD-006 的报表基于一次有明确范围的快照。输入指定 UTC 半开区间、事实截止时刻、统计群体，以及供给与成本数据的完整性。所有报表均可从同一份去标识化输入重新计算。开发验证使用测试商户与开发积分。

```sh
cargo run --locked --offline -p orange-control-plane --bin orange-market-report -- docs/market-metrics/sample.snapshot.json
```

[供需指标字典](supply-demand.md) · [收入与成本](economics.md) · [最小事件合同](events.md) · [对账样本](sample.md) · [预期输出](sample.report.json)

独立工具读取本地快照，不连接数据库或渠道。生成快照的主体需具备原数据的读取权限。真实数据库验收通过现有受控支付、作业、计量和结算路径产生开发事实，再由只读事务取数。报表不包含客户标识、节点凭据、提示词、音频或交易密钥。
