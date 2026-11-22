# 本机 HTTP 指标

orange-user-api、orange-admin-api 和开发用 orange-api 可在独立回环端口提供 Prometheus 拉取端点。启动对应进程前设置 `ORANGE_METRICS_BIND=127.0.0.1:9411`，随后访问该端口的 `/metrics`。同机多个 API 分别配置端口。省略变量时，API 保持原有启动路径。非法值、通配地址或已被占用的端口会导致启动失败。指标端口只提供本地进程级统计，具备主机访问权限的其他进程也可读取。

| 指标 | 定义 |
|---|---|
| orange_http_responses_total | 响应头已生成的请求数，按 API 角色、有限的 HTTP 方法集和状态码类别累计。 |
| orange_http_response_duration_seconds | 请求进入路由至响应头生成的时间，以秒计。长流后续传输时间另计。 |
| orange_http_active | 正等待响应头的请求数，按 API 角色统计。客户端取消时释放计数。 |

所有 API 实例独立持有注册表和 SDK provider。指标采集不会读取请求体、文件名、路径参数、查询参数、凭据或外部传入的标签。静态标签限制了时间序列的数量。重启会重置进程指标。扣款和管理员决策仍由账本及审计记录证明。

OpenTelemetry API、SDK 和 Prometheus 导出器固定为 0.33.1，Prometheus Rust 客户端固定为 0.14.0。四项均使用 Apache-2.0 许可证，注册表源码和校验值以 Cargo.lock 为准。[上游导出器发行记录](https://github.com/open-telemetry/opentelemetry-rust/blob/main/opentelemetry-prometheus/CHANGELOG.md)

在服务端执行 `python3.12 scripts/check-http-observability.py` 可构建并启动三个真实 API，分别核验公开路由、指标端口和脱敏。开发身份配置与测试库可通过 `--private-dir` 指定。分布式追踪、外部采集器、告警响应和生产容量另有验收。
