# 本地依赖就绪检查

在拥有开发配置的服务端仓执行 `python3 scripts/development-readiness.py --start --wait-seconds 30`。启动器在仓库的 .local 目录协调并发启动，依次使用已有的 PostgreSQL 与 Keycloak 管理脚本。已有健康实例可以直接使用。仅查询时运行 `python3 scripts/development-readiness.py`，也可用 --environment 指向已有开发配置。

| 资源 | 默认端口或分配方式 | 账户与权限 |
|---|---|---|
| PostgreSQL | 127.0.0.1:55432，数据库 orange_dev | 本地账户 orange_local。探针以只读事务核对库名、用户、服务实际地址、端口和数据目录。业务测试可创建独立短生命周期数据库。 |
| Keycloak 开发身份服务 | 127.0.0.1:44180，orange-dev realm | 用户和运营各有独立 client 和回调来源。探针检查公开发现文档和签名公钥。登录、令牌交换与 MFA 另做端到端验收。 |
| 用户与运营前端 | HTTPS 回环 43122 和 43112 | 私有配置分别位于 identity/user-config.json 和 identity/admin-config.json，应用内部角色由服务端判定。 |
| S3 兼容测试存储 | check-s3-source.py 使用已审核的 SeaweedFS 和临时端口 | 独立 fixture、临时凭据、专属 bucket，测试结束回收。此基础探针只检查常驻数据库和身份服务。 |
| Redis | 可丢失缓存或通知依赖；实际开发核心路径使用 PostgreSQL outbox/inbox | 本入口未配置或验证 Redis 实例。 |

退出状态 0 表示已就绪，1 表示依赖不可用，2 表示配置不合要求。输出仅含公开状态。--wait-seconds 在临时故障时重试，配置错误立即返回。启动失败可单独运行 `scripts/local-postgres.py start` 或 `scripts/local-user-identity.py` 查看对应服务的诊断。健康检查不需要真实商户或 GPU。
