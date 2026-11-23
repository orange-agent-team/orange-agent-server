# 本地 CI 检查矩阵

从经过评审的服务端工作树运行 `python3.12 scripts/development-ci.py --job matrix` 查看检查项及所需资源。

| 作业 | 输入与权限 | 通过条件 |
|---|---|---|
| quick | 可提供 --candidate 和本地贡献提交的完整 SHA。协调器、期望的协议、镜像和工具锁来自可信工作树。仅三个待审源文件与固定工具被传入非 root Linux 容器。容器禁网、无主机挂载、无设备、无 Docker 套接字，根文件系统只读 | rustfmt、Clippy、领域测试、Worker 和 Host 描述符指纹均通过 |
| clients | 使用可信服务端源码，显式传入 --desktop、--web、--admin。需要本地工具缓存与开发身份服务 | 从空目录重建 Rust 与 Dart 绑定，核对协议，再运行四仓的独立构建与原生探针 |
| stateful | 可信源码、专属本机服务，临时租约及测试身份 | 新库和新 realm 冷启动，执行受控的无 GPU Worker 往返，销毁并重建开发命名空间，旁路数据保持不变 |
| faults | 与 quick 相同的隔离环境 | 正常样本通过。分别注入格式差异、会失败的 Rust 断言及协议变更，检查容器真实的非零退出码和失败证据 |

所有作业默认读取本仓 .local 下的固定输入，可用 --environment 指向预置的本地环境目录。首次准备 Linux 输入时，运行既有的 prepare-linux-images.py、prepare-ci-tools.py 并提供 linux-platform-arm64.lock.json 锁定的 protoc 归档。可信开发作业的基础工具与依赖准备见[工具链资格](../engineering/toolchain-qualification.md)和[本地资源](../engineering/development-environment.md)。

作业按顺序执行必需步骤，任一失败即返回非零。仅全部通过时产生带源提交与检查名称的完成记录。每次 Linux 快速检查都创建一个空的容器临时文件系统。删除编译产物不会改变依赖解析；重新取得缺失的源包或工具后仍须校验对应锁文件。一般贡献不需要生产令牌和 GPU。含原生代码生成、数据库配置或容器引擎访问的作业只在候选提交已经得到信任后运行。远端仓库的必选检查和分支保护需在配置远端后另行验证。
