# Orange Agent 模型清单检查器

在 macOS ARM64 上解压后，从本目录运行：

```sh
./bin/orange-model-manifest list
./bin/orange-model-manifest show whisper-small-ct2-int8-cpu-v1
./bin/orange-model-manifest validate < models/whisper-small-ct2-int8-cpu-v1.json
./bin/orange-model-manifest verify whisper-small-ct2-int8-cpu-v1 /absolute/path/to/transcription-model
./bin/orange-model-manifest verify-proposed /absolute/path/to/cache < proposed-manifest.json
```

`show` 返回固定版本的清单和规范化内容摘要。`verify` 与 `verify-proposed` 读取指定缓存目录内的文件；每个文件的大小及 SHA-256 都匹配后才输出校验回执。查看 [ModelVersion 结构](contracts/json-schema/model-version.v1.json)、[用户 API](contracts/openapi/model-manifests.v1.json) 和 [源码构建说明](docs/model-version-manifests.md)。

模型权重由操作员另行提供。此工具读取本机数据，不下载模型、登记市场报价或授予调度权限。
