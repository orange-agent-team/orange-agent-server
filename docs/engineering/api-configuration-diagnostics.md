# HTTP API 配置与诊断

开发 API、用户 API 与 Admin API 在打开数据库、启动模型计量或绑定端口前解析公共配置。同一字段的优先级从低到高依次为内置缺省值、JSON 配置文件、环境变量、命令行。配置文件由 ORANGE_API_SETTINGS_FILE 或更高优先级的 --config 选择。JSON 只接受下表的字段。

| 字段 | 环境变量 | 命令行与约束 |
|---|---|---|
| environment | ORANGE_ENV | --environment。当前服务要求显式 development |
| bind | ORANGE_BIND、ORANGE_USER_BIND 或 ORANGE_ADMIN_BIND | --bind。仅回环；端口 0 分配临时端口 |
| metrics_bind | ORANGE_METRICS_BIND | --metrics-bind。off 或独立回环地址 |
| region | ORANGE_REGION | --region。允许 LOCAL、US、JP、CN-MAINLAND、CN-HK，默认 LOCAL。该标签描述部署配置，实际数据驻留仍有独立资格要求 |
| diagnostic_log | ORANGE_DIAGNOSTIC_LOG | --diagnostic-log。on 或 off；默认关闭结构化请求日志 |
| identity_file | ORANGE_DEV_IDENTITIES_FILE、ORANGE_USER_CONFIG_FILE 或 ORANGE_ADMIN_CONFIG_FILE | --identity-file。对应角色的既有身份文件 |
| database_url_file | ORANGE_DATABASE_URL_FILE；已有部署可继续使用 DATABASE_URL | --database-url-file。文件中存放数据库连接串；Unix 要求该文件没有组或其他用户的访问位。同一有效环境层同时给出连接串和文件会报冲突 |

配置文件中的相对路径以文件所在目录为基准，命令行及环境变量中的相对路径以工作目录为基准。`orange-user-api --config local.json --describe-config` 可检查解析结果和每个字段的来源；该模式完成基本配置及身份载荷校验后退出，不连接数据库。报告仅包含角色、已校验的监听地址、诊断开关、区域及来源。可选的连接器、模型和商户继续通过各自既有配置管理。

```json
{"environment":"development","bind":"127.0.0.1:43103","identity_file":"identity/user-config.json","database_url_file":"database-url","region":"LOCAL","diagnostic_log":"off"}
```

## 程序化错误处理

启动失败以结构化 JSON 写入标准错误，包含 code、class 和 retryable。配置错误另有 field、source。将字段和值分开可以定位到输入来源，同时避免把路径、URL、令牌、请求体或解析器回显写入错误日志。

| HTTP 错误类别 | 客户端处理 |
|---|---|
| input | 修正请求，参考原响应体中的领域错误码 |
| permission | 核对会话、主体和当前授权 |
| quota | 核对预算、余额、限额或速率限制 |
| conflict | 按原幂等键核对现有资源或操作 |
| unavailable | 读取可按策略退避；变更操作先核对原结果 |
| internal | 记录服务端诊断 ID，汇入运营排查 |

响应头 x-orange-error-class 和 x-orange-retry-class 提供枚举值。既有结构化错误保留原响应体与已有的重试语义，例如余额不足保留 never。响应头不授予重放权限。Admin 会话、step-up、审批和审计的原有 request_id 继续独立处理。

## 请求到执行租约

每个 HTTP 响应的 x-orange-diagnostic-id 由服务端生成。打开 diagnostic_log 后，http_completed 记录角色、请求 ID、状态码，以及成功提交作业的脱敏 job 标识。Worker 网关可用 ORANGE_DIAGNOSTIC_LOG=on 记录 assignment_committed。只有数据库提交成功后才写该事件。两个事件通过 job 标识关联，后者还包含 attempt 和 node。不同标识使用不同的哈希输入前缀。随机标识的摘要可供排查关联，低熵标识的摘要仍应作为内部诊断数据保护。

进程内的诊断队列最多容纳 256 项，已满时调用方继续处理业务；后续记录的 dropped_before 用于观察丢弃量。该输出可缺失，计费与审批以持久账本和审计记录为准。授权 HTTP 路由、mTLS 租约与受控输入的联合测试可通过 `python3.12 scripts/check-api-diagnostics.py --section correlation` 重现。
