# 开发包依赖与许可清单

运行以下命令检查已经生成的 macOS 开发包。工具需要 Python 3.12、构建时固定的 Rust 1.97.1、本地 Cargo 依赖缓存，以及包含发行源码提交的服务端仓库。输入为压缩包旁边的构建回执。输出为独立 JSON 文件，重复运行会复核既有内容。

```sh
python3.12 scripts/audit-development-distribution.py --receipt "$BUILD_RECEIPT" --output "$INVENTORY_FILE"
```

检查器验证压缩包大小、SHA-256、归档成员、包内 manifest 与固定提交的 Cargo.lock。对每个实际随包 wheel，记录版本、摘要、元数据声明的许可、通知文件、内含原生库及嵌套发行元数据。Rust 部分由固定源码、目标和 feature 生成，区分第一方组件和第三方 crate，并单列构建依赖所进入的解析图。动态系统库和全部递归原生依赖的法律审查仍需单独完成。

部分第三方 wheel 有许可声明，却未随附独立许可文件。打包程序按确切包名和版本，从仓库内核验过的上游通知目录挑选原文；每一份保留 Git 提交、blob ID、字节数及 SHA-256。输出的 notices/sources.json 只列入本包实际使用的补件。此前生成的压缩包保持原内容摘要，需配套相应版本的通知文件。第一方工程尚未声明对外分发许可。[通知来源](../distribution/notices/sources.json)
