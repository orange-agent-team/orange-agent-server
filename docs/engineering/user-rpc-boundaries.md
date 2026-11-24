# 用户 RPC 的调用主体、金额和重连

用户接口的操作编号、输入输出及错误结构以各自的 OpenAPI 为准。授权目录将模型、价格、作业、对象和个人账户的 35 个操作绑定到 13 类访问边界。SDK 或桌面桥可以用操作编号定位契约，无需从 URL 猜测权限。[授权目录](../../contracts/registry/user-rpc-authority.v1.json)

| 入口 | 可用主体、限制与返回值 |
| --- | --- |
| 模型清单与可报价模型 | 当前浏览器会话可查询固定清单和摘要。可报价目录另外核对当时可用的文本报价与执行配置，目录读取不会锁定容量。四种模态的本地清单及检查器另行验收。[模型清单](../../contracts/openapi/model-manifests.v1.json) · [可报价目录](../../contracts/openapi/models.v1.json) |
| 价格与报价 | 文本报价绑定 owner、预算、不可变输入版本与固定价格，输出上限为 1 至 4096。转写报价还绑定目标节点及当时有效的音频输入授权。读取原报价资金来源需要当前权限。确认作业时重新校验预算与报价。[文本价格](../../contracts/openapi/pricing.v1.json) · [转写报价](../../contracts/openapi/artifacts.v1.json) |
| 作业与取消 | 客户端通过请求号和原 invocation 读回不确定结果。个人和团队查询都在当前 owner 范围校验原提交主体。新客户端携带上次观察到的作业版本，过期取消返回 HTTP 412；原请求的幂等记录按原内容核对。正在运行的作业还需要独立的终止与结算事实。[作业与对象](../../contracts/openapi/artifacts.v1.json) |
| 对象上传与结果 | 上传元数据绑定字节数、类型及摘要，内容使用独立二进制请求，通用上限 104857600 字节，音频输入上限 100000000 字节。内容下载和结果读取重新检查对象版本与授权。[对象结构和边界值](../../contracts/json-schema/artifacts.schema.json) |
| 钱包和预算 | 浏览器只能以当前个人账户会话读取个人钱包。桌面 FRB 的 readPersonalWallet 与 readPersonalBudgets 使用对应的 wallet:read 和 budgets:read 凭据，目标服务固定，分页受限。余额、预留和费用用十进制微积分字符串传输。[账户契约](../../contracts/openapi/account.v1.json) |
| 事件与重连 | 每个连接有独立 connection_generation，跨连接只继承已应用的 stream_id 和序号。Web 页面撤下旧状态、旧取消按钮和旧结果，按快照或最新状态读回恢复。原作业仍由服务端处理。控制消息 2048 字节、数据帧 65536 字节、单帧 ACK 等待最长 5 秒。[事件协议](owned-job-events.md) |

浏览器写请求需要一个与会话配置一致的 Origin、一个 CSRF 令牌及一个幂等键。重复相同或不同值都属于歧义，服务端先拒绝，再保留原作业、冻结、事件和幂等记录。幂等键采用既有的 1 至 128 字节 Orange 标识符字符集。未认证、权限不足、输入无效、冲突及服务不可用按具体操作的错误响应返回，客户端读取 code、request_id 和 retry_class 后按原请求核对。

执行 `python3.12 scripts/check-user-rpc-boundaries.py --web-repo ../orange-agent-web --desktop-repo ../orange-agent-desktop` 可以交叉核对服务端规范、Web 快照和桌面锁定的只读账户投影。作业事件的真实 HTTP、WebSocket 和双用户界面回归分别通过 `scripts/check-owned-job-history.py` 与 `scripts/check-job-state-preconditions.py --section browser` 执行。当前验收中的 OAuth 用户会话与预算采用开发配置。
