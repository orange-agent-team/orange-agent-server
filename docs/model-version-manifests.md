# 固定模型版本清单

`GET /api/v1/model-manifests` 列出随服务交付的参考版本和清单摘要。`GET /api/v1/model-manifests/{version}` 返回对应清单。两个接口均需要有效的用户会话，遵循 User API 的 `Cache-Control: no-store` 策略。读取清单不会创建执行档案、报价、Job 或下载授权。可调度版本和当次报价仍由已有市场目录与服务端审核记录决定。[API 契约](../contracts/openapi/model-manifests.v1.json)

每份清单记录上游仓库和提交、许可标识、每个文件在上游和本地缓存中的相对路径、字节数及 SHA-256。原生引擎固定源码与构建脚本；Python 引擎固定依赖锁和适配代码摘要；OCI 描述符使用镜像摘要。接口类型、目标设备、资产总字节数和定价引用均为版本内容。当前四份内置清单的定价策略为 `local_unpriced`；有价提议必须显式携带价格版本 ID。定价引用本身不会开通收费。[ModelVersion schema](../contracts/json-schema/model-version.v1.json)

文件按上游路径排序。验证器拒绝跨目录、路径冲突、大小写别名、重复 JSON 字段和额外执行字段。清单摘要取完整 ModelVersion 的 RFC 8785 规范化 JSON 字节的 SHA-256。修改一项权重或适配器需要重新计算清单摘要。

从锁定源码构建 E04 的 `orange-model-manifest` 后，可以离线检查清单或核对已有文件。以下示例使用已准备好的 ASR 缓存。

```sh
cargo build --offline --locked -p orange-model-runtime --bin orange-model-manifest
target/debug/orange-model-manifest list
target/debug/orange-model-manifest show whisper-small-ct2-int8-cpu-v1
target/debug/orange-model-manifest verify whisper-small-ct2-int8-cpu-v1 .local/transcription-model
target/debug/orange-model-manifest validate < proposed-manifest.json
target/debug/orange-model-manifest verify-proposed /absolute/path/to/cache < proposed-manifest.json
```

`verify` 从操作员选定的目录读取真实文件，以文件句柄核对大小和摘要并拒绝符号链接，返回本次观测到的文件数与字节数。`validate` 只回传格式及引用验证通过的描述符。发布清单由 `scripts/generate-model-manifests.py` 从固定锁核对，显式传入 `--write` 才更新快照。运行 `python3.12 scripts/check-model-version-manifests.py` 可以检查四份真实缓存、篡改样例和一次性本地数据库上的用户接口。
