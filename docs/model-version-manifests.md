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

### 本地文本和嵌入适配器

运行 `orange-model-manifest adapter VERSION` 可读取当前随包清单的运行时版本、清单摘要、接口和上限。文本适配器使用 GGUF 内嵌模板，通过私有引擎的模板计数接口检查输入和输出是否能放入上下文。Chat Completions 与 Responses 均逐次提交完整输入并关闭提示复用，Responses 同时关闭响应存储。嵌入适配器只接受文本输入，按实际清单限制批量、每项 token 数及向量维度。当前固定的 ONNX 版本允许每批 32 项、每项最多 512 个 token，返回 384 维有序向量。

本地选择文本或向量传输需要匹配清单中的引擎与接口。平台已审核的私有文本别名沿用原有的显式文本配置。图像、转写和未知版本不会取得向量路由。读取清单不改变平台模型的审核、供给或价格状态。

### SDXL 固定模板

在解包后的 macOS 开发包中执行 `bin/orange-image --describe-template` 可离线读取模板。回执包含模型清单和模板各自的 SHA-256、执行环境锁、请求字段、精度、调度器、种子范围、设备类型及输入限制。生成入口核对同一份编译时模板及 Python 适配器。请求只提供提示词和种子；UTF-8 字节数及两个 CLIP 编码器的 token 上限分别校验。模板读取不启动 Python 或加载权重。

### 查询本地权重缓存

在模型检查工具包中运行 `bin/model-fetch config/local-model.lock.json /path/to/private-cache --inspect`，可以只读检查内容寻址的 GGUF。没有缓存目录时返回 `absent`；下载或导入的暂存文件按各自的字节数列出；只有重新核对固定大小、文件头、完整 SHA-256 和当前文件身份后才返回 `verified`。查询不创建缓存、不发起下载，也不修复损坏的权重。

`read_lease_observed` 表示是否观测到该已发布文件上的协作锁。通过 ModelCache 取得的 VerifiedArtifact 在持有期间保留共享锁。这个字段不能作为全机推理进程数或 GPU 已卸载的依据。

断点下载将已批准的源地址、重定向白名单、内容摘要、大小及文件头绑定到私有暂存记录。记录中只保存哈希。重新启动时，只有匹配的分片才会发送 Range 请求。远端返回不同对象标签、失效范围或新的完整对象时，缓存从零重新校验。旧版缺少来源信息的不完整分片会重新获取，完整的旧暂存文件可通过文件头和完整 SHA-256 离线恢复。已验证的权重仍可在断网时使用。
