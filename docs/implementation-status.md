# 实施状态与本机验证

日期：2026-11-04。这是G0/P1/P2/P3的局部实现，**不是完整G3或G6候选版**。

## 已实现

- orange-domain：整数金额、规范字符串、计价/费率、预算限额、取消/完成互斥、旧响应和GPU回收事实检查。
- orange-protocol：严格DTO、JSON金额无损传输、请求内容摘要、调用purpose与输入校验。
- orange-control-plane：真实PostgreSQL迁移、根预算、已存在Quote的原子提交/来源分桶冻结、同键/同Invocation去重、排队取消与原来源释放、平衡/不可变账本与outbox。
- orange-agent-runtime：真实SQLite持久意图、响应提交屏障、批准绑定/到期/撤权、独占Host锁、重启后unknown禁止重放。
- 本地开发HTTP入口：预算/Job查询与创建、仅排队任务取消。只允许显式development和loopback；开发令牌不代表OIDC或生产身份。

没有实现的部分仍PENDING：生产身份/团队、价格/模型注册到Quote的真实计价、模型执行、Job运行中取消/结算、Worker/实际GPU、完整工具/连接器、Web云Host、支付、Admin服务及G4—G6。当前HTTP不会伪造这些功能成功。

## 本机运行

先安装PostgreSQL17并运行`python3 scripts/local-postgres.py start`，创建的集群仅位于本仓库.local/postgres且监听127.0.0.1:55432；随机凭证写入忽略目录，不输出到日志。停止使用`python3 scripts/local-postgres.py stop`。

验证：`python3 scripts/check.py --postgres`。脚本明确使用已安装Rust工具链并检查rustc 1.97.1，运行格式、单元/协议/SQLite恢复、Clippy和实际PostgreSQL并发测试。缺数据库时集成测试失败，不悄悄跳过。ORANGE_RUST_TOOLCHAIN可选择同版本已安装工具链。

HTTP二进制orange-api还需显式ORANGE_ENV=development、DATABASE_URL及ORANGE_DEV_IDENTITIES_FILE。身份文件是owner_id/token数组，令牌至少32字符；owner和Quote必须来自受控开发初始化，未提供公开充值/造余额/伪报价接口。真实身份/报价切片完成前，本入口仅用于开发验证。

## 版本与环境记录

当前锁定Rust 1.97.1、SQLx0.9.0、Axum0.8.9、Tokio1.54.1；rusqlite从候选0.41.1调整为0.39.0，因为SQLx工作区与其sqlite3原生links范围冲突。实际依赖版本以Cargo.lock为准，兼容变更记录保留。

Homebrew在当前macOS上安装PostgreSQL时服务注册失败；使用实际安装二进制及补齐缺失的share/lib链接后，专用集群成功启动并完成真DB测试，没有注册系统启动服务。安装过程更换的OpenSSL默认链接已恢复为原OpenSSL3，PostgreSQL继续使用其版本化OpenSSL4依赖。

Flutter官方归档清单返回404，官方GitHub稳定分支读取超时；Docker守护进程不可用。它们记录为环境阻塞，仅影响相应任务，不作为其他代码未实现的借口。没有生产服务、商户或真实GPU验证。
