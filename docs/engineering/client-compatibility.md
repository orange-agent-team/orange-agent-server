# 客户端兼容性验收

## 已固定的最低读者

HTTP 作业样本来自真实开发库中的单个个人作业。旧服务完成冻结，新服务在升级后处理取消与撤权。样本保留排队、已取消和 401 三个响应，仅保存 Content-Type、Cache-Control 与作业版本响应头。最低作业客户端来自用户 Web 提交 `c0ffde5b93de4a3310f0113d841c6c0216e2ab6c`；比较读者和服务端提交见[固定响应记录](../../contracts/fixtures/http-job-v1/job-history.json)。

旧客户端忽略新增的 X-Orange-Job-Version，仍按原 schema 验证正文和作业归属。当前客户端可以从响应头取得版本，接受旧服务省略该头的响应。只分发版本号不会赋予取消权限。消费者使用新增版本号时，写入仍走服务端条件取消和当前用户权限校验。金额在两代客户端均保持十进制字符串。

运行 `python3.12 scripts/check-minimum-job-clients.py --web-repo /path/to/orange-agent-web`，会从指定仓库的两个固定提交独立导出源码、用锁文件离线安装，再把同一批真实响应交给各自的 ComputeClient。临时传输层只负责回放 HTTP。测试还使用超过 JavaScript 精确整数范围的合成边界值，并检查未知权限字段与错误作业身份都受到拒绝。被比较的两个本地 Git 对象和该 Web 仓库的私有 npm 缓存是这项历史验收的输入。

## 对应的执行边界

| 范围 | 真实依据 |
|---|---|
| Worker 最低与现行控制版本 | 对照两个 schema 的固定二进制字节，现行 Rust 客户端解码，并通过 mTLS 验证负例及会话不变。见[控制协议](../worker/control-protocol.md)。 |
| 同键同参、同键异参、费用过期 | PostgreSQL 验证已提交作业回执及冻结。写入跨越报价有效期时回滚；按 owner、operation 和 key 区分幂等空间。见[事务与保留](../database/transactions-and-retention.md)。 |
| 断线与重放 | 当前用户权限、HTTP 快照、WS 游标、连接代次、过期 reset 和单帧确认分别验证。二进制内容从授权对象端口读取。见[作业事件](owned-job-events.md)。 |
| 生成物漂移 | `scripts/check-wire-reproducibility.py` 从两份独立源码产生描述符与 Rust 绑定，并实际引入漂移检查失败。Web/Admin 的 schema 对照各自固定的服务端提交；桌面使用成对的 FRB 绑定摘要。 |
