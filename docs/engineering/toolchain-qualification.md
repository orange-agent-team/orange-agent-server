# 四仓工具链与依赖资格

开发机与本地 CI 使用同一个入口。运行 inventory 会解析实际 Cargo 元数据、npm 锁和本机工具版本，输出组件的版本、注册表校验值和上游声明的许可证。pins 输出开发 OCI 镜像和模型的不可变标识。reproduce 从各仓的 Git 提交导出四份干净源码，各自在新的构建目录运行。资格报告保留四个提交、锁文件摘要及本次构建产物摘要。

```sh
python3.12 scripts/qualify-toolchain.py --section inventory --desktop ../orange-agent-desktop --web ../orange-agent-web --admin ../orange-agent-admin
python3.12 scripts/qualify-toolchain.py --section upgrade --desktop ../orange-agent-desktop
python3.12 scripts/qualify-toolchain.py --section pins
python3.12 scripts/qualify-toolchain.py --section reproduce --desktop ../orange-agent-desktop --web ../orange-agent-web --admin ../orange-agent-admin
```

| 组件 | 锁定来源与验证 | 运行条件 |
|---|---|---|
| Rust 服务端 | rust-toolchain.toml 与 Cargo.lock；验证 Tokio、Axum、Tower、Tonic、Prost、SQLx、OpenDAL、OpenTelemetry 与 Prometheus 的实际解析结果 | macOS ARM64 当前验证；Linux ARM64 由独立镜像和制品验证 |
| 桌面桥及 Flutter | 桌面 Cargo.lock、runtime.lock.json、bridge-tools.lock.json、flutter.lock.json 和 pubspec.lock；核对 Flutter revision、Dart、FRB 与 cargo-expand 的真实版本 | 本次构建 Apple Silicon 原生动态库，并运行 Flutter 的预算提示组件测试。完整桌面应用及模型资格有单独的发布验收 |
| 用户 Web 与 Admin | 独立 package-lock.json，同一固定 Node 版本。逐一确认直接依赖与开发依赖的版本和 npm integrity | 分别离线 npm ci 后运行各自的生产构建命令 |
| 模型后端 | llama.cpp 提交、模型仓库 revision、权重长度和 SHA-256；Diffusers、faster-whisper、ONNX Runtime 使用各自受管环境的版本和锁 | 本地 CPU 后端与凭据分别发布。vLLM、SGLang 和多 GPU 的生产部署资格仍需目标环境证据 |
| 容器 | 构建、运行与测试数据库镜像按内容摘要引用。索引与镜像配置摘要随目标架构固定。gVisor 的版本和归档摘要见 cloud-isolation.lock.json | 在本机已有隔离 Linux 环境验证。云区域身份、驻留与容灾单列 |

## 升级与回退

1. 从已记录的提交创建候选分支，先归档旧的版本清单、模型或镜像摘要，以及当前协议和客户端包的凭据。
2. 在负责该组件的仓库修改版本和锁文件。生成器升级同时检查描述符、Rust 绑定和 FRB 接口变化。upgrade 从干净的桌面源码重新生成 Rust 与 Dart 绑定，逐项比较输入和输出摘要，再运行跨版本协议检验。旧客户端对新增字段、未知枚举和撤权行为需要重跑兼容测试。[Worker 和云 Host 协议回归](protocol-upgrades.md)
3. 在干净源码目录运行上述资格命令，并保留 git 提交和产物摘要。涉及驱动或引擎的修改另跑对应 GPU 上的准入、推理和回收测试，缺设备时保留待验证状态。
4. 出现 ABI、协议或模型结果回归时，恢复成套的编译器、生成器、依赖锁与模型摘要，重新验证旧提交。涉及数据库迁移时依照升级兼容证据处置数据，避免对已有用户数据直接降级。
