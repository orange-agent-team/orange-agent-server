# 版本化对象存储与下载授权

User API 可从 `ORANGE_ARTIFACT_S3_CONFIG` 指定的私有 JSON 文件读取 S3 配置。原本使用 `ORANGE_ARTIFACT_ROOT` 的本地文件配置继续可用。每个进程选择一种后端。已有文件系统根保留自己的数据库标识，切换后端需要另行迁移已有内容。

```json
{
  "endpoint": "https://storage.example.invalid",
  "region": "us-east-1",
  "bucket": "orange-private",
  "prefix": "development/artifacts",
  "credentials_file": "/private/orange/artifact-credentials.json"
}
```

配置及凭据文件由运行账户持有并设置为 0600。凭据格式为 `access_key`、`secret_key` 和可选的 `session_token`。生产形态的端点使用 HTTPS；开发测试允许显式回环 IP 上的 HTTP。为该根分配专属私有前缀。写入账户需要查询桶版本配置，以及该前缀的读写、复制、列举版本和精确版本删除权限。只读服务使用各自的低权限凭据，须能查询版本配置及读取同一前缀的根标记和已固定对象版本。

| 进程 | 配置变量 |
| --- | --- |
| User API 与内置 Worker 写入路径 | `ORANGE_ARTIFACT_S3_CONFIG` |
| 独立 Worker Gateway 只读路径 | `ORANGE_ARTIFACT_S3_CONFIG` |
| 云 Host Gateway | `ORANGE_CLOUD_HOST_ARTIFACT_S3_CONFIG` |
| Admin 支持读取 | `ORANGE_ADMIN_SUPPORT_S3_CONFIG` |

第一次打开根时由写入服务确认桶已启用版本控制，在 PostgreSQL 中保存根标记身份，在对象提供商中以条件写入创建对应标记。重启及读取会核对固定标记版本和内容。相同数据库和同一存储根只允许一个写入进程持有 PostgreSQL 会话锁。

现有上传接口保留原有 upload_id、容量预留及确认流程。S3 写入取得具体暂存版本，校验声明的 SHA-256 后从该版本复制到独立发布键。服务端固定复制响应中的目标版本，重新读取该版本并核对内容，最后通过现有数据库事务检查当前授权、删除状态和传输有效期。读取、分段读取和删除均使用保存的提供商版本。

已登录客户端向 `POST /api/v1/owners/{owner}/uploads/{upload}/download-authorizations` 提交 `version_id`，并发送 Origin、CSRF 及幂等头。只有当前可读且已发布的版本可以获得 GET 链接。Worker 生成的结果还需与成功作业的交付引用相符。重试必须使用原请求键，回复中的到期时间不会因此后移。客户端应核对版本、字节数及 SHA-256，并将返回的 URL 作为临时凭据处理。

停止授权或逻辑删除后，平台拒绝新签发。已签发的链接最多 60 秒后无法发起新的读取，已经获得的响应和下载副本不能收回。上传端直连 PUT 与 Worker 租约签发需各自通过接入验收。
