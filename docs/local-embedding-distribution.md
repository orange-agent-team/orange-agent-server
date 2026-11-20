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
