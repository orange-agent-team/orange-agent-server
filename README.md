# Orange Agent Server

**代码实施已开始：** 当前领域、协议、PostgreSQL资金预留切片与SQLite操作恢复已有代码及本机测试，见[实施状态](docs/implementation-status.md)。完整G3/G4—G6尚未完成；下方原文为设计背景，不代表所有工程仍只有文档。

日期：2026-11-04。服务端仓库承载E01 Domain、E02 Protocol、E03 Agent Runtime、E04 Model Runtime、E07 Control Plane、E08 Worker、E09 Python Backends，共7工程；另有桌面2、用户Web 1、Admin 1，总数11。

E03源码移入本仓库，提供共享库及独立云Host入口；桌面固定版本引用。E07不在API进程内执行Agent工具，而是通过受控协议分配云Run和持久化其事实。云Host与公共GPU Worker分开：后者始终只做审核模型推理。

四个目录已初始化为独立本地Git仓库，已有应用清单和部分实现；下方文档保留设计背景。管理前端为独立E11，消费本服务端管理API。当前管理写入边界见[节点操作与审批实现](docs/admin-actions-implementation.md)，完整计划仍未完成。

- [管理控制面契约](docs/admin-control-plane.md)：Admin身份、动作、审批、持久化与业务调用。
- [管理操作索引](docs/admin-api-index.md)：路径、请求/响应schema、权限与动作政策。
- [Admin OpenAPI草案](contracts/openapi/admin.v1.json)：管理API的权威机器可读定义。
- [Admin JSON Schema](contracts/json-schema/admin.schema.json)：类型与命令校验模型。

平台管理后台与用户组织管理使用不同主体/权限。云工作区限额内暂不单独收费，模型消费仍走原BudgetRoot/Quote/账本。新增Web/云Host/Admin工作量需另细化，不能沿用旧任务数量宣称全部已覆盖。

实施选型、用户确认的范围、待实测事实和外部上线条件见[产品决策登记](docs/product-decisions/README.md)。

算力市场的供需指标、原币种成本与去标识化事件合同，以及可重算的样本报表见[市场指标与对账](docs/market-metrics/README.md)。
