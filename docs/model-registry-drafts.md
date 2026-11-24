# 模型草稿登记

开发平台范围的计算管理员可以在独立 Admin 会话中登记 ModelVersion 清单。写入要求 `models.read`、`models.draft`、有效 Origin、CSRF、近期二次认证和幂等键。清单经过与离线模型检查器相同的结构和语义验证。草稿登记完成后，模型编号、清单及摘要不可更换；目录权限与会话在访问时重新校验。

| 接口 | 作用 |
| --- | --- |
| POST /admin/v1/model-drafts | 登记版本并写入审计；首次返回 201，原主体按原幂等键重试返回 200 |
| GET /admin/v1/model-drafts/{id} | 读回清单、规范化摘要、登记主体与请求摘要 |
| GET /admin/v1/models | 按版本编号分页读取当前范围的已登记条目 |
| GET /admin/v1/models/{id} | 读回该版本的状态、模态及版本摘要 |

登记请求摘要沿用 `orange-request-v1`，操作名为 `admin-model-draft-registration.v1`，载荷为顺序固定的数组，依次是登记主体、幂等键、清单摘要。管理员可在响应丢失后用原编号读回并核对这三个绑定条件。

迁移 86 将已存在的可信执行档案名称保留在原有命名空间。登记草稿和旧供给程序写入执行档案时由 PostgreSQL 裁定同名竞争。已存在执行档案的编号不能登记为草稿；草稿编号不能被旧程序插入执行档案。新草稿本身不生成作业、价格或执行许可。许可审核、兼容性证据、独立批准与正式激活分别验收。

运行 `python3.12 scripts/check-model-registry-drafts.py --environment /path/to/local-development` 可验证一次性版本 85 数据库升级及独立 Admin HTTP 会话中的登记、拒绝和并发写入。该验收使用开发会话夹具。
