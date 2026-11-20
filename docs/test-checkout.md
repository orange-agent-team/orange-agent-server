# Stripe 测试充值与订单恢复

此开发入口允许个人账户和组织 Owner、Billing 使用明确启用的测试商户档位。客户端提交档位和订单 UUID，金额、币种、购买微积分和测试商户由服务端确定。组织 Admin 可以按产品规则申请财务操作，直接充值要求 Owner 或 Billing。当前 HTTP 入口处理执行操作。用户 Web 的个人账户页提供创建、取消未派发订单、打开测试收银台及核对原订单的操作。[接口契约](../contracts/openapi/test-checkout.v1.json)

Unix 开发用户 API 通过 `ORANGE_STRIPE_TEST_CONFIG` 指向当前账户的私有 JSON 文件。字段为 `merchant_id`、`api_key` 和 `webhook_secret`。生产密钥无法启用此适配器。缺少配置时服务不挂载充值和回调路由。档位来自 `test_credit_offers`，价格字段为 Stripe 测试 Price 标识，现金使用币种最小单位，积分使用微积分。用户界面不接收卡号。API 版本固定为 `2026-11-18.endive`，当前接受普通单商户的测试卡 Checkout。真实商户资格、地区、方式和支付认证单列。

创建本地订单不会联系商户。用户确认派发时，服务先核对测试账户，再次检查当前用户及范围权限后原子领取一次派发权。平台在 Checkout 和 PaymentIntent 上附加同一订单和服务端生成的派发标记。明确尚未派发时可取消。已发出请求但响应不确定时保留原订单并等待权威对象；同一订单不会触发第二次创建会话。用户从收银台返回只会查询当前账户下的原订单。[Stripe Checkout 会话创建](https://docs.stripe.com/api/checkout/sessions/create) · [Stripe v1 幂等行为](https://docs.stripe.com/api/idempotent_requests)

回调入口使用未改动的请求体核验时间戳和 HMAC-SHA256，限制请求体大小并支持多个 v1 签名。通过签名且与本地派发相关的事件先写入不可变 Inbox，再返回接收确认。事件金额不会直接入账。后台从同一商户重新读取 Checkout、档位明细、PaymentIntent 和已捕获 Charge，核对来源绑定、测试模式、币种、现金金额、退款及争议标志。只有完整匹配的成功付款才通过同一 PostgreSQL 事务增加购买余额并写入平衡分录与唯一履约凭证。重复与乱序事件可以查询同一付款。商户不可用时保留待核对工作，身份或计量证据不一致时进入复核。[Stripe 履约流程](https://docs.stripe.com/checkout/fulfillment.md?ui=full-page) · [原始请求体验签](https://docs.stripe.com/webhooks) · [Checkout 明细](https://docs.stripe.com/api/checkout/sessions/line_items)

本机验收命令为 `python3 scripts/check-test-checkout.py --web-repo ../orange-agent-web`。测试使用真实用户 HTTP、PostgreSQL、浏览器和隔离的本地商户协议替身。Chrome 丢弃会话派发后的成功响应，刷新后确认仍为同一订单，随后由后端复核入账。测试不会打开外部收银台。未覆盖真实商户沙箱凭据，原路部分退款、拒付冲正及长期财务对账仍有独立交付要求。

## 原购买来源与部分退款

59 号迁移按购买履约、钱包分录和 Job 冻结记录重建逐单余额。每笔购买分别保留可用、Job 冻结、已消费、退款冻结和已退五项。Job 优先消耗原有未关联购买余额，再按购买入账顺序分配到具体订单，部分扣费和取消都回到原来冻结的购买来源。本机 Worker 验收使用独立的历史购买凭证夹具：对 10,000 微积分的原单执行真实转写，结算后原单已消费 2,200；发布前撤权的另一单在同一回收流程结束后全部恢复。

60 号迁移支持原支付币种的测试退款。界面按现金最小单位输入退款额，同一订单同时只允许一笔未决申请。申请先冻结原单可用购买微积分，保留已消费和 Job 冻结的部分。累计退款现金为 R、原购买积分为 C、原现金总额为 A 时，对应积分为 `ceil(R × C / A)`。本次冻结量为前后两个累计值之差。现金最小单位不足以对应正的可退积分时拒绝申请。失败或未派发取消时沿原单解冻。确认成功后保留原交易，另记独立的平衡冲正分录。

用户在个人账户的测试订单记录中可找回原购买并申请退款。后台对账事项提供原订单、冻结积分、退款金额及不可变条款摘要。独立审批人核对同一份资料，执行授权只产生待派发记录。实际请求前再次核对批准权限、原 Charge 的币种、金额、已退累计额和争议状态。同一退款使用永久业务编号，已派发但结果未知时只查询原 Charge 下符合原绑定的 Refund；分页游标可持久恢复。签名通知只唤醒查询。原退款对象处于 pending 或 requires_action 时继续冻结，权威对象的最终成功或失败才改变账本。对账事项的关闭另走既有治理流程。[Stripe 退款状态](https://docs.stripe.com/api/refunds/object) · [退款创建](https://docs.stripe.com/api/refunds/create)

联合验收使用 `python3 scripts/check-test-checkout.py --web-repo ../orange-agent-web --admin-repo ../orange-agent-admin`。用户浏览器验证退款申请、刷新恢复、取消及原订单历史。两套独立 Admin 浏览器对同笔受保护的退款执行制单、复核和批准。测试另检查撤销审批权限、外部失败、并发派发、签名通知重放和回包丢失后不重复发送。商户响应全部来自本机协议替身。
