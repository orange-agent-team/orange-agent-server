# CN-03 Notion 只读数据源

控制面用 `ORANGE_NOTION_SOURCES` 加载当前进程所有者的私有文件。每项包含 source_id、owner_id、token、credential_revision 和 registration.roots。每个根由业务别名标识。页面根固定 page_id；数据库根固定 database_id 及一个或多个 data_sources。服务端只向 https://api.notion.com/v1 发送读取请求，固定使用 Notion-Version 2026-03-11。数据库查询通过 data_sources/{id}/query 发起。[版本说明](https://developers.notion.com/reference/versioning) · [数据源查询](https://developers.notion.com/reference/query-a-data-source)

创建运行时必须显式选择 allow_document_connectors。仓库访问选择与文档访问选择分别记录。用户随后核对来源摘要，选定页面或数据库根及有效期。既有外部连接器的每 Run 总次数和并发上限同时适用。每次向服务商发送 HTTP 请求之前，以及最终交付之前，控制面都会核对会话、Run、选定根、未撤销的授权及当前调用期限。

search_pages 在已授权页面根中按标题查找，按登记页分页，页中可能没有匹配项。read_page 可读取明确登记的页面，或来自同一授权查询的数据库行。list_blocks 可沿本机签发的子节点句柄浏览，深度上限为 16。同步块、链接及其他外部目标不会产生可遍历句柄。query_database 核对数据源仍属于登记的数据库，再允许筛选和排序。标题、正文、数值、复选框及状态等以类型化字段返回；关系、附件链接等未投影属性显式标记。提及标记保留其位置；服务端不解析其目标。

分页游标和导航句柄以 HMAC 绑定来源注册信息、授权及目标，重复使用时还要重新验证对象身份和父级。内容移出授权范围、删除、账号拒绝或限流会中止读取。认证拒绝将停用该注册摘要，恢复需要新的凭证版本和用户授权。真实 Notion 工作区认证需单独留存证据。

```sh
python3 scripts/check-notion.py --database-url-file .local/database-url --identity-config .local/identity/user-config.json
```

加入 `--web-repo ../orange-agent-web` 可通过真实 Chrome 访问四个动作、读取查询结果中的授权行，并在撤权后刷新核对。验收首先在一次性 PostgreSQL 中重建 47 版的仓库授权、调用和文件回执，然后升级并重复迁移，逐项核对原数据与文档外发选择。源端没有真实工作区令牌时，这些测试使用固定版本的本机协议夹具。

迁移回归先以固定的 schema 47 User API 通过真实 HTTP 创建原 Run 的外部连接器授权。更改原请求的授权标志会被旧程序拒绝。原进程退出且数据库仍为 47 后，测试保留先前授权、撤销、调用和回执，再执行当前迁移及其重试，验证旧许可与新动作边界。随后当前处理器执行实际的 Notion 授权和撤销期间读取验收。第三方服务使用受限测试替身。历史生产者源码提交为 8ea1e3df71c3f7f90c32986cd564b5c637d13e21。首次运行从 Git 对象离线构建；缺少旧 Git 历史的检出可传入 `--schema47-source-archive` 并核对固定摘要。
