# CN-04 Confluence Cloud

一个登记项绑定固定的 atlassian.net 站点和 cloudId、空间 ID 与空间 key，以及精确的附件交付主机清单。控制面从私有文件 `ORANGE_CONFLUENCE_SOURCES` 读取 source_id、owner_id、credential_revision、registration 和 credential。支持站点 API token、Cloud scoped API token 和 Cloud OAuth bearer 这三种宿主已取得的凭证。使用 Cloud API 时，路径固定在该 cloudId 的 Confluence 产品下。集成首先对站点的 tenant_info 进行无凭证核验；每次访问外部接口前都会检查当前 Run 授权。

search_pages 使用固定空间条件构造 CQL，按标题匹配。read_page 返回 ADF 中的惰性文本、未投影节点类型以及附件元数据。list_children 读取直接子项，对页面重新核对其空间、父级与版本。导航和分页凭据绑定原授权。read_attachment 从受批准页面的附件清单选择固定版本，先在私有对象根预留容量，再下载至独立 quarantine 键。附件重定向仅限同一 API 的指定页面下载路径或预先登记的交付主机；跨主机下载使用无凭证请求。[搜索接口](https://developer.atlassian.com/cloud/confluence/rest/v1/api-group-search/) · [内容层级](https://developer.atlassian.com/cloud/confluence/rest/v2/api-group-children/) · [附件版本](https://developer.atlassian.com/cloud/confluence/rest/v2/api-group-attachment/)

隔离附件以 application/octet-stream、attachment、nosniff、no-store 交付。模型与普通用户产物 API 无法通过上传 ID 读取。下载前重新验证租户、空间、源版本、摘要和授权；失效后维护程序回收实际文件及容量。隔离与内容哈希不构成恶意软件扫描认证。

验收在本机独立模拟站点及媒体主机，真实使用 PostgreSQL 和 OpenDAL 私有目录。核对站点凭证不发往媒体主机，验证 3 MiB 隔离文件、容量拒绝、在途撤权、幂等回执和旧版本不得恢复。真实 Atlassian 企业站点、云 OAuth 授权和各地区媒体域名另行认证。

```sh
python3 scripts/check-confluence.py --database-url-file .local/database-url --identity-config .local/identity/user-config.json
```
