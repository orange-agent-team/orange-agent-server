# 托管 Run 的首个文本模型回合

托管工作区可使用已审核价格执行一个纯文本问答回合。用户在 /agent 选择推理价格和输出上限，先预留工作区，再核对真实计量器给出的最高费用。点击确认后，服务端才冻结开发积分并把作业交给独立推理 Worker。纯文本输入、单条用户消息和空工具列表是此版本支持的模型请求形状。工作区内的用户输入与模型使用同一个不可变文件版本。

## 接口和权限

创建 Run 时可附加 model，包含 price_version、model_version、invocation_id 和 output_limit。模型模式引用 application/json 的 GenerateRequest。旧客户端省略 model 时继续使用原来的 text/plain 工作区协议和幂等摘要。已有报价、作业、撤回记录或其他 Run 已使用的调用标识不能再绑定。

| 操作 | 验收条件及效果 |
| --- | --- |
| GET /api/v1/agent/runs/{id}/model | 本人可查看该回合的模型、预算、计量投影和模型作业编号，阶段由工作区、响应和源对象的当前状态决定。 |
| POST /api/v1/agent/runs/{id}/model/quotes | 使用近期有效的 Host 心跳、租约和准备凭据，核对确切输入后由可信计量器生成报价。输入、输出分别按每千单位的微积分计价。重复请求使用原键。 |
| POST /api/v1/agent/runs/{id}/model/submit | 要求用户会话、Origin、CSRF、幂等键和报价确认三项，即报价编号、请求投影摘要和最高微积分数额。已确认作业与不可变输入绑定、预算冻结及钱包记录在同一 PostgreSQL 事务内提交。 |
| GET /api/v1/agent/runs/{id}/model/result | 只有可信 Worker 的响应已提交、用量已核对、账本已结算并且原输入仍然可用时，才返回当前用户可读取的已提交答复。 |

普通用户模型接口继续处理自己创建的标准报价。绑定到云 Run 的报价需要从该 Run 的确认入口提交。云 Host 的 mTLS 权限不授予它用户钱包权限。每个 Run 对应至多一个此类首轮模型作业，后续工具与多轮交互仍需单独实现。[云控制面契约](../contracts/openapi/cloud-agent.v1.json) · [定价契约](../contracts/openapi/pricing.v1.json)

## 停止、回收与数据删除

用户取消或撤回、租约到期及 Host 停机都会围住该 Run 的后续效果。已绑定但尚未开始的模型作业通过原有预算和钱包释放逻辑结清为零；已开始的任务进入 cancelling，仍需原有设备回执与实际用量判定结算。已提交并结算的答复保留原始账务事实。用户删除模型输入后，Run 结果读取以及派生产物的直接内容读取都会重新检查来源权限，页面清除原答复。物理工作区继续通过原分配的 Linux 清理证据释放容量。

## 本机重验

```sh
python3 scripts/check-cloud-model-turn.py \
  --database-url-file .local/database-url \
  --identity-config .local/identity/user-config.json \
  --web-repo ../orange-agent-web \
  --model-runtime-root .local/model-runtime
```

需要本地固定的 Linux ARM64 gVisor 工具包、通过校验的 llama.cpp 可执行文件及 GGUF、Node 22.23.2 和既有开发身份服务。测试在单独数据库生成开发积分、用户会话和推理价格，调用本机真实模型，以及独立运行的可信计量器，并核对云工作区和推理进程的各自回收记录。测试不建立商户交易，也不认证外部 GPU 或第三方支付。

联合测试还会在浏览器中丢弃已提交的确认回包，核对随后恢复的原始模型作业。对另一笔报价，测试单独阻住开发推理节点的启动窗口，再从同一个 Run 取消尚未开跑的已预留作业，核查其扣款为零、钱包恢复、Worker 未获启动许可。数据库在提交时校验云模型作业同时具有不可变输入绑定和对应的 artifact_pins 记录，缺少任何一项都会回滚整笔冻结。
