# CN-05 已登记 PostgreSQL 视图

源适配器位于 E07 的 `pg_views` 模块。宿主提供专用读取连接池、管理员审核的注册表，以及本次用户授权的视图 ID 集合。源适配器支持 list_views、describe_view、query_view。每次请求核对当前数据库、登录角色、视图 OID、定义摘要和审核列类型。输入使用视图 ID、列名、一个等值过滤条件和分页参数。源凭证归宿主管理。

分页按登记的唯一、非空 bigint 键排序，支持升序和降序。最大 1000 行，响应大小最多 1 MiB。文本过滤值使用参数绑定。每条语句超时 5 秒，整次动作超时 30 秒。返回数据标记为外部不可信内容。调用方需要在读取前后独立校验当前 Run 的授权和有效期。

开发源验收创建一次性 PostgreSQL 数据库和专用登录角色，覆盖真实分页、过滤、越权列、角色撤权、超过 1 MiB 的单行以及并发更换视图定义。源查询等待视图锁后重新读取已提交的定义，在定义不匹配时终止。

```sh
python3 scripts/check-pg-views.py --database-url-file .local/database-url --identity-config .local/identity/user-config.json
```

开发 User API 通过 `ORANGE_PG_VIEW_SOURCES` 读取当前进程所有者的私有常规文件。每个条目登记 source_id、owner_id、固定回环 IP 和端口、源密码及 registration。注册表包含源数据库、只读登录角色和按 ID 编排的视图。审核人固定 schema、关系名、relation_oid、pg_catalog 搜索路径下的定义摘要、公开列类型与分页键。配置不进入客户端快照。远端数据源 TLS 与企业密钥托管留待独立验收。

用户在运行详情中读取可授权资源，提交源摘要、视图 ID 集合、有效期与幂等键。每次读取需要当前用户会话、相同运行、有效的输入产物及未撤销的授权。每 Run 最多 100 次读取，允许同时保留 4 个请求。动作在独立源连接池执行，交付前再次检查同一授权；在途撤权会丢弃读取结果。重复的动作键返回 409；重新授权产生新授权 ID。查询输出中的 bigint 及分页游标编码为十进制字符串。

接口与载荷见 [CN-05 OpenAPI](../contracts/openapi/pg-connector.v1.json) 与 [JSON Schema](../contracts/json-schema/pg-connector.schema.json)。联合测试使用真实的开发 OIDC 发现和两套隔离数据库，其中用户会话和已运行的 Run 是显式测试记录。

平台会记录源端登录、专用角色属性或 SELECT 权限的明确拒绝，后续请求在连接源前即被拦截。恢复源端权限并重新加载相同登记不会恢复旧 Run 授权。管理员确认源已修复后可设置新的 credential_revision，用户核对新的登记摘要并重新授权。
