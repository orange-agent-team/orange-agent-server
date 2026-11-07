# 当前用户 Job 取消

POST /api/v1/owners/{owner}/jobs/{job}/cancel 接受严格空 JSON 对象、当前用户 Cookie、Origin、CSRF 和稳定 Idempotency-Key，返回最新 JobView。没有 owner 开发 Bearer 回退、客户端退款金额或资源释放声明。

同事务验证当前会话、账户、owner/组织成员及原 submitting_user_id；组织 admin 不因此获得其他提交者的停止权限。原提交者降为 viewer 或失去预算提交权/源内容 ACL 后，仍可停止自己的已接受任务；离开组织、账户或会话撤销后不能借重试绕过当前授权。billing 角色没有本接口的内容域权限。源已删除不阻止原提交者停止任务，接口不读取源正文。

排队取消沿既有原来源桶零费用释放路径；运行中仅 running→cancelling、原请求键与持久事件/outbox 同提交，保留钱包冻结、input pin 和原 attempt。CPU 当前授权监督发现取消后等待 owned Child 退出，再走独立云清理与未交付账本终结。未获得清理证据不能标为已释放。

取消与输出提交竞争锁定同一 Job；提交已经胜出时保留 succeeded/原usage/原收费，迟到取消只读取终态。取消胜出后，旧启动、续期、输入和发布不得继续获得授权。重复取消只返回当前事实，不产生二次退款或取消事件；事务写事件失败整体回滚，原键可重试。

开发 Bearer 的 cancel_queued 仍保持原先队列范围。当前不开放管理者取消他人任务、移除成员后的用户入口、GPU硬件认证或真实支付操作。协议测试中的CPU退出报告为夹具；实际模型流程另列。

状态：正式源码专项、严格Clippy和关联统一回归通过，USER-JOB-CANCEL-001已单独记DONE；不代表原父任务或全路线完成。验证命令：`python3 scripts/check-user-cancellation.py`。
