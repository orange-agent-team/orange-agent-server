# 模型草稿登记

开发平台范围的计算管理员可以在独立 Admin 会话中登记 ModelVersion 清单。写入要求 `models.read`、`models.draft`、有效 Origin、CSRF、近期二次认证和幂等键。清单经过与离线模型检查器相同的结构和语义验证。草稿登记完成后，模型编号、清单及摘要不可更换；目录权限与会话在访问时重新校验。

| 接口 | 作用 |
| --- | --- |
| POST /admin/v1/model-drafts | 登记版本并写入审计；首次返回 201，原主体按原幂等键重试返回 200 |
| GET /admin/v1/model-drafts/{id} | 读回清单、规范化摘要、登记主体与请求摘要 |
| GET /admin/v1/models | 按版本编号分页读取当前范围的已登记条目 |
| GET /admin/v1/models/{id} | 读回该版本的状态、模态及版本摘要 |

列表要求显式传入平台范围。详情操作允许只传入环境，服务端仍会核对平台权限。若指定范围类型，则必须同时指定范围编号。

登记请求摘要沿用 `orange-request-v1`，操作名为 `admin-model-draft-registration.v1`，载荷为顺序固定的数组，依次是登记主体、幂等键、清单摘要。管理员可在响应丢失后用原编号读回并核对这三个绑定条件。

迁移 86 将已存在的可信执行档案名称保留在原有命名空间。登记草稿和旧供给程序写入执行档案时由 PostgreSQL 裁定同名竞争。已存在执行档案的编号不能登记为草稿；草稿编号不能被旧程序插入执行档案。新草稿本身不生成作业、价格或执行许可。许可审核、兼容性证据、独立批准与正式激活分别验收。

运行 `python3.12 scripts/check-model-registry-drafts.py --environment /path/to/local-development` 可验证一次性版本 85 数据库升级及独立 Admin HTTP 会话中的登记、拒绝和并发写入。该验收使用开发会话夹具。

## 开发环境的模型认证

迁移 87 保存签名报告、启用决定和停用决定。报告绑定模型清单和权重、执行配置、运行时、实测硬件、许可文字、实际推理结果及有效期。使用 `scripts/sign-model-certification.py --report <report.json> --license-notice <notice> --signing-key <绝对路径>` 对核对过的报告离线签名。签名密钥为只对持有人开放的 Ed25519 私钥。Admin 服务仅加载公钥。部署时将 32 字节原始公钥的小写十六进制文本放到受保护的普通文件，在启动前用 `ORANGE_MODEL_REVIEW_PUBLIC_KEY_FILE` 指定绝对路径，随后重启服务。缺少可信公钥时登记认证和启用都会返回不可用。

| 操作 | 验收要点 |
| --- | --- |
| POST /admin/v1/model-drafts/{id}/certifications | 开发平台范围、有效二次认证、CSRF、原幂等键，以及匹配当前模型的签名 |
| GET /admin/v1/model-drafts/{id}/certifications | 显式平台范围，默认每页 20 条，最多 100 条，游标绑定主体、权限与模型 |
| GET /admin/v1/model-drafts/{id}/certifications/{report_id} | 返回原签名、许可文字和审核事实，并记录访问审计 |
| model.activate | 申请包含清单版本和报告编号。另一管理员批准后，执行时再次检查双方权限、报告有效期和清单版本 |
| model.disable | 按现有单操作员策略核对版本和权限后停用。已有作业和冻结金额继续按原流程核对 |

认证到期或停用会阻止新执行授权。已审核的执行配置只接受签名覆盖的权重和执行摘要。使用固定价格的清单只允许对应价格，local_unpriced 模型不能参与有价调度。迁移前已有的可信配置以升级时的精确身份保存。

运行 `python3.12 scripts/check-model-governance.py --environment <本地开发目录> --runtime <本地固定模型目录>` 可在一次性 PostgreSQL 库中验证签名、双人批准、CPU 推理、开发积分冻结和停用。验收中的许可审阅人为测试身份，真实许可审查和生产发布资格须另外核验。
