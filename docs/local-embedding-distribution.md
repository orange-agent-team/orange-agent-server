# macOS ARM64 嵌入运行时开发包

构建机使用 Rust 1.97.1、macOS SDK、Python 3.12、uv 与已缓存的锁定 Cargo 和 Python 构建依赖。在服务端仓库根目录运行以下命令。`RUNTIME_COMMIT` 为已验收的完整服务端提交。`E09_WHEEL` 是同一提交通过 `scripts/package-embedding-backend.py` 生成的 wheel。`MODEL_CACHE` 包含已核验的 ONNX 模型和分词器。准备目录中的所有第三方 wheel 必须能匹配该提交的 uv.lock。

```sh
mkdir -p .local/wheelhouse
uv export --project backends --locked --no-dev --extra embedding --no-emit-project --no-header \
  --format requirements-txt --output-file .local/wheelhouse/dependencies.txt
python3.12 -m pip --isolated download --index-url https://pypi.org/simple --only-binary=:all: \
  --no-deps --require-hashes --dest .local/wheelhouse -r .local/wheelhouse/dependencies.txt
python3 scripts/build-embedding-release.py --commit "$RUNTIME_COMMIT"
python3.12 scripts/package-local-embedding-release.py \
  --native ".local/embedding-release/$RUNTIME_COMMIT" --wheelhouse .local/wheelhouse \
  --backend-wheel "$E09_WHEEL" --uv-cache .local/uv-cache --model-cache "$MODEL_CACHE" \
  --output-dir .local/embedding-distributions
```

发行脚本从固定源码重建第一方 Python wheel，逐个校验第三方 wheel 摘要，比较两次原生构建和两次归档，然后在临时目录解包并离线安装。只有实际本地模型通过回归，才保留以摘要命名的压缩包。旁附 JSON 记录提交、摘要、向量维度、用量及回收结果。权重独立管理，构建阶段在本机完成，不需要模型服务账号。

开发主机收到客户端断开通知后，终止该请求对应的 CPU 子进程并等待退出。并发请求在原执行槽仍被占用时收到 HTTP 429。如果操作系统无法确认退出，主机关闭该槽，后续请求收到 HTTP 503。运行超时返回 HTTP 504，内存分配或后端失败返回 HTTP 503。每次推理使用独立子进程。

嵌入包的烟雾测试会启动解包后的二进制，断开一个正在执行的请求，检查原 PID 已被回收。另一次请求向原 Python 适配代码注入分配失败，随后使用同一主机和实际 ONNX 权重重新生成 384 维向量。该项覆盖失败恢复；测试不需要耗尽物理内存。
