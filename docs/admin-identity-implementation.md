# 管理身份与只读后端实施记录

2026-11-04。范围为 P1/P2/P5 的一个可运行切片，不代表整个 Admin 或 G3 完成。

## 实际实现

`orange-admin-api` 使用独立 OIDC 客户端与管理 Cookie。登录经过 Authorization Code、S256 PKCE、随机 nonce/state、浏览器绑定和五分钟一次性交换屏障。固定 issuer 的发现、JWKS 和 token HTTP 请求限制同源、不跟随重定向、十秒超时及一 MiB 响应上限。`openidconnect 4.1.0` 验证签名、issuer、audience、nonce、过期和可用的 at_hash；服务端另校验配置的 ACR 与最近 auth_time。

平台权限只来自 PostgreSQL 中显式授权的 issuer/subject，身份提供方角色不能自动成为平台角色。每次请求检查主体状态、会话绝对/空闲过期、授权环境、作用域、有效期和撤销。不同作用域的角色不做笛卡尔积；0.1 会话结构暂以所有作用域权限交集表示，服务端仍保留逐作用域权限。总览和操作列表在返回前再次检查当前授权。

Cookie 固定 `__Host-`、Secure、HttpOnly、SameSite=Lax、Path=/，服务端仅保存会话随机值的 SHA-256。管理登录与用户开发 Bearer 身份相互独立。登出与 step-up 需要精确 Origin、CSRF 和请求键。step-up 绑定原会话与主体，成功后撤销原会话。失败的代码交换不自动重试。

总览查询现有真实 Job、管理操作投影、节点和对账事项表；操作列表按租户、动作领域权限及稳定游标过滤。读事件写入不可变审计表。节点/对账表的业务生产流程和管理动作的创建、批准、执行尚未实现，不会伪造执行成功。

## 重复运行

环境：Rust 1.97.1、Node 22.23.2、PostgreSQL 17.11、已有专用 Colima `orange-agent`、本机 Chrome、Admin npm 锁定依赖。将对应 Node 加入 PATH 后，从服务端仓库执行：

```sh
python3 scripts/check-admin-identity.py --admin-repo ../orange-agent-admin
```

这是显式跨仓库验收参数，不是客户端构建时隐式引用相邻源码。脚本构建后端、启动或复用专用数据库/身份容器、创建临时 API 与 HTTPS Vite 进程、执行真实测试并清理自己启动的 API/Vite。共享开发数据库和身份容器保留，可分别执行 `local-postgres.py stop`、`local-identity.py stop` 停止。端口占用时拒绝冒用未知进程。

Keycloak 固定 `26.8.1` 官方镜像摘要 `sha256:3f3c5f8e03c614774b4bd9173b88391fefa4153bfc010d8012d59ad825377332`。只映射 `127.0.0.1:44180`，不挂载主机目录，限制 1 CPU/1536 MiB。两个随机测试身份、OTP、客户端秘密和 bootstrap 密码仅保存于权限受限的 `.local/identity`，不提交、不打印。该开发 realm 使用真实密码和 TOTP 的 LoA 流程，未使用固定 ACR 伪装双因子。HTTPS 测试证书只对 loopback 有效，不写系统信任库。浏览器测试不录制 trace、视频或登录截图。

## 验证与限制

- 三项真实 PostgreSQL/路由授权集成测试：跨作用域角色不混合、财务操作过滤、游标隔离、撤权、会话/授权到期、主体禁用、CSRF/Origin 和审计不可变。这里的会话为明确的数据库测试夹具，不能独立证明 MFA。
- 三项真实 Chrome/Keycloak 流程：错误 TOTP 拒绝，正确双因子到实际总览，跨环境/租户拒绝，撤权立即生效，登出；PKCE/nonce/state、跨浏览器和重复回调拒绝；真实签名的仅密码 ACR1 被拒绝建立管理会话。
- 现有 Admin 替身浏览器测试仍单独保留，用于 UI 缓存与错误行为；不会混称真实身份认证。

本切片支持开发部署的 platform:platform 与 tenant 读取；region/resource 范围明确拒绝。生产入口仍关闭。JWKS 热轮换、入口限流、完整拒绝审计、企业身份认证、实际硬件 MFA、管理动作/独立批准、价格草稿、支持授权等仍是后续任务。当前公钥变化会失败关闭，需要重新发现，不宣称已完成生产密钥轮换。Keycloak 开发 H2 数据库不属于生产部署方案。

参考固定版本[认证流程源码与官方说明](https://github.com/keycloak/keycloak/blob/26.8.1/docs/documentation/server_admin/topics/authentication/flows.adoc)、[LoA 实现](https://github.com/keycloak/keycloak/blob/26.8.1/services/src/main/java/org/keycloak/authentication/authenticators/conditional/ConditionalLoaAuthenticator.java)、[Rust OIDC 验证库](https://docs.rs/openidconnect/4.1.0/openidconnect/)。
