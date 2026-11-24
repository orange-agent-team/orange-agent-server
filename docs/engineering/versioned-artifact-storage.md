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

停止授权或逻辑删除后，平台拒绝新签发。已签发的链接最多 60 秒后无法发起新的读取，已经获得的响应和下载副本不能收回。Worker 的租约签发仍需单独验收。

## 用户直传与断线恢复

客户端先通过已有接口建立带有 upload_id、长度和 SHA-256 的上传记录。取得逻辑 version_id 后，可在该上传记录下使用以下接口。团队写入需要当前成员和内容写权限。Worker 输出有单独的租约授权边界。

| 方法 | 上传记录下的相对路径 | 用途 |
| --- | --- | --- |
| POST | `/upload-authorizations` | 以稳定请求键和 version_id 取得五分钟暂存 PUT；已发布时返回当前可用版本。 |
| GET | `/upload-authorizations` | 根据原 upload_id 恢复当前请求者最近的传输及原请求键。 |
| GET | `/upload-authorizations/{transfer}` | 查看具体传输、已观测或已确认的提供商版本，不返回签名 URL。 |
| POST | `/upload-confirmations` | 以 transfer_id 作为幂等键，提交逻辑 version_id 和明确的 provider_version。 |

PUT 仅允许写入平台分配的暂存键，签名包含准确的 Content-Length、SHA-256 校验头、If-None-Match 和归属元数据。首次成功后，对同一键的重复 PUT 会被条件写拒绝。调用方应先查询原传输，使用观测到的版本进行确认。状态 `unconfirmed` 表示平台还没有观测到完整版本，网络中断时不能据此断定提供商未收到请求。

确认在 PostgreSQL 中固定源版本，按该版本读取和校验，通过 CopyObject 生成独立发布版本。复制响应丢失时，重试核对已有目标的归属、版本、长度和完整摘要。只有原传输及当前权限仍有效，才会写入 available。重新提交同一上传或恢复确认不会产生新的任务或账本分录。

暂存容量一直保留到已签发 PUT 到期。清理使用带归属的零字节条件写标记封住该键，按确切版本删除暂存数据，确认后才归还这部分容量。标记只包含传输用随机标识，不包含文件内容。发布副本按独立保留策略处理。
