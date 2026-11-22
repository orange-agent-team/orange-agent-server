# 工程、运行进程与验收

服务端维护共享执行库、协议及数据库迁移。四个产品仓库分配 11 个核心工程。已提交的 [清单](workspace.json) 给出工程编号、包名、相对路径、每个服务端二进制的所属包及允许的依赖。新增 Rust 包或可执行入口会导致清单检查失败，需要一起评审其职责和权限。

| 工程 | 运行位置、接口与状态所有者 |
|---|---|
| E01 领域库 | Rust 进程内值类型。金额和状态转换由领域类型校验，当前包没有第三方或工作区依赖。 |
| E02 协议库 | 为调用方提供版本化消息和序列化。gRPC 特性用固定 Protobuf 编译器生成传输类型。 |
| E03 Agent Runtime | 在受控宿主执行循环，SQLite 持久化本地操作、审批及恢复状态。桌面构建使用导入并校验的源码归档。 |
| E04 模型 Runtime | 库负责选后端和监督进程。本地模型进程通过私有套接字报告就绪，由主管理进程确认退出和释放租约。桌面链接监督逻辑，推理后端运行在独立进程。 |
| E05 桌面桥 | FRB 导出 Rust API，连接 Flutter 与当前固定版本的 E03/E04。独立 Cargo 工作区读取仓内 vendor 和工具锁。 |
| E06 桌面应用 | Flutter 页面、桌面事件流及原生窗口。在账户、模型调用和工具授权前展示当前状态，经桥调用宿主。 |
| E07 控制面 | 对用户、Admin、公共 Worker、受管云 Host 提供不同的认证入口。任务提交、预算冻结及账本由同一 PostgreSQL 所有者在事务内写入。只读离线报表是同一包的 CLI。 |
| E08 公共 Worker | 执行已审核的推理配置，持久记录 Worker 租约和执行状态，经控制连接返回结果。云工作区执行进程归 E07 的云 Host 部署。 |
| E09 推理后端 | 独立 Python wheel，为受管模型进程提供适配。基础 wheel 可离线构建，两次构建逐字节核对；转写、向量、图像运行依赖分别按宿主与模型资格验证。 |
| E10 用户 Web | 从仓内锁定契约构建静态资源，以用户会话调用服务端。 |
| E11 Admin | 独立静态包和认证来源，通过 Admin API 执行审批、审计和运营操作。 |

文件系统适配包 orange-platform-files 是内部 Rust 支持库。进程清单另列开发 CLI、测试替身和报表工具，部署时仅为需要的入口分配权限。开发 API 和夹具的启动不能证明实际商户、GPU 或生产身份可用。

## 依赖审查

检查器读取 Cargo 解析后的清单，按包、路径和依赖种类核对。所有 workspace 内的普通、条件编译及构建期依赖都进入有向图。开发依赖另查，例如控制面测试借用 Worker 做集成验证。E08 的生产依赖不能引入 E07 的数据库实现。E03 的生产执行接口不反向依赖 E04。网页契约既校验本地 SHA-256，也与服务端固定提交中的原始字节比较。桌面单独验证 vendored 源码、内容清单及桥实际解析的路径。

## 基础验收

使用 Python 3.12、各仓工具锁及本地依赖缓存，在服务端仓执行下列命令。跨仓参数用于验收已经构建的四个独立仓。普通客户端构建只需它自己的源码、工具和锁文件。

```sh
python3.12 scripts/check-workspace-architecture.py --section inventory
python3.12 scripts/check-workspace-architecture.py --section edges
python3.12 scripts/check-workspace-architecture.py --section builds
python3.12 scripts/check-workspace-architecture.py --section clients --desktop ../orange-agent-desktop --web ../orange-agent-web --admin ../orange-agent-admin
python3.12 scripts/check-workspace-architecture.py --section client-builds --desktop ../orange-agent-desktop --web ../orange-agent-web --admin ../orange-agent-admin --evidence ../services_plan/execution/evidence
python3.12 scripts/check-workspace-architecture.py --section roles --database-url-file .local/database-url
```

builds 逐个检查七个服务端 Rust 包，然后检查整个 workspace 的全部目标和特性，并从固定 E09 源码两次离线构建 wheel。client-builds 单独检查 E05，再核对当前三个客户端提交的开发包、双构建凭据和归档摘要。roles 在 Unix 上启动真实子进程与套接字，验证客户端退出、错误身份和资源回收；模型载荷为固定测试夹具。三项数据库用例在临时 PostgreSQL 库验证并发抢占预算、原桶解冻、失败无副作用和借贷平衡。数据库帮助器检查连接已关闭后回收专属测试库。
