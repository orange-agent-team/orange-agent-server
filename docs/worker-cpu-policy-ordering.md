# CPU策略停止的时序验收

CPU owner先接收节点心跳/策略，再验证输入及续约权限。这些是独立的真实RPC，不能假设策略变化一定在心跳之前发生。

| 实际顺序 | 要求 |
|---|---|
| 策略先提交，随后心跳收到当前generation/命令 | 取消模型，取得当前NodeStop认证并应用到Broker，独立确认进程退出；原drain/quarantine断言完整保留 |
| 心跳先完成，策略在后续执行权限检查前提交 | 权限验证失败使ControlConnection失效，停止模型且无结果提交；不能把失败推断为已应用节点策略，也不能复用失效执行连接 |

原fresh policy application失败与第二种顺序相容。已有missing-command测试明确保留第二种路径的安全边界；不能通过放宽权限、设置active=true或假造policy receipt来使第一种用例通过。

本变更只为真实集成测试加入显式时序控制：test-control-gates在BeforeHeartbeat/AfterHeartbeat按唯一command ID等待测试释放；测试须确认同步点和真实模型Generating均到达，才提交真实Admin意图fixture并释放请求。RPC响应、mTLS、PG、Broker应用及模型/进程行为仍是真实执行；不是固定成功替身。

同步模块仅通过control-plane的real-worker-tests特性启用。默认Worker构建无该特性（evidence/policy-default-features.json）；没有运行时参数、网络接口或模型工具可以启用同步点。生产授权逻辑未改动。

policy-ordering-real-worker-fixed.log：9项真实CPU测试全部通过，包含保持原断言的drain/quarantine及新增明确的execution-first场景。严格Clippy通过。第一次新增fixture使用了错误的kind字符串，修正为现有node.drain协议值后重验，失败日志保留。

本验证不把本地退出当成云端Admin动作完成，也不证明后续重连或长期可用性。执行连接失效后仍需新的合法连接/控制授权，不能因为有一个待投递命令就修改整机贡献策略。
