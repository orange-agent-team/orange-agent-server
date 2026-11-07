# 云端CPU清理确认与执行终态

本切片连接实际CPU退出、本机Broker释放、云端attempt关闭和下一次分配。它不是GPU探针或远程硬件证明；服务端信任当前登记设备运行经过审核的CPU适配器，并核对不可混用的原执行身份。不能把该协议当作抵御恶意主机所有者的TEE认证。

## 本机与协议

SQLite schema6按原command保存不可变CpuCleanupEvidence，包含准确grant、实际stopped ProcessRecord及原cleanup digest。只有CPU所有者已等待模型任务、确认owned Child退出并通过本机Broker.release后才写入；不通过PID扫描、空目录或任意JSON构造已清理事实。诊断PID只参与原记录摘要，不用于恢复或杀进程。

首次清理请求的outcome和digest在发送前持久化。后续即使输出回执恢复、会话变化，也不改变原请求身份。云回执写入失败时重新发送原事实，不重新启动模型或重复释放。

Hello/Welcome独立协商cpu_cleanup_version1，使用ConfirmCpuCleanup RPC。清理不要求输出writer；可用cleanup-only会话核对旧执行。当前设备认证必须有效，原ACK/Lease/journal/grant/epoch/generation必须匹配；旧epoch仅作为历史绑定，不恢复执行或续期。

PG0025引入不可变reviewed_cpu_profiles，完整约束后端SHA、模型大小、上下文和启动配置，重新计算原执行profile digest。没有CPU准入、设备标记不是cpu、进程不是stopped/cpu_only、原实例或grant不匹配均拒绝。GPU不能使用此路径。

## 云端终态与金额

云端在节点、预算/Job、attempt顺序下锁定原执行，保存唯一清理回执和事件，将该attempt改为closed，从未决占用索引中退出。重复请求返回原回执；不同实例/内容不能替换。不会把节点drain/quarantine改成online，也不会自动完成Admin动作。

如果ResponseCommit已存在，云端优先保留succeeded事实，不能因调用方丢失回执而退款。若本机只知道delivery_unknown，服务端原子检查提交是否存在；没有提交且已关闭原attempt后，旧上传不能再发布。

对确认未交付的failed/cancelled任务，独立事务按原held buckets释放冻结额并记录唯一release账本、零费用资格和Job终态，不伪造“测得0 usage”。资源回执可先成功、金额处理后失败；重放/维护仅完成原金额事务，不重复释放资源或余额。有可信已交付usage则沿用原报价结算；未计量的历史成功结果不猜测收费。

只有云清理确认且Job已结算终结，才移除该Job准确的input/output pins。其他消费者pin不受影响。PG0026记录系统派生删除：源已删除的输出由维护流程标记deleted，不伪造用户删除身份；物理回收仍遵守保留期和最后一个pin，实际文件删除与quota恢复使用既有受管I/O机制。

## Host接线与边界

开发Worker默认confirm_cleanup=true。输出成功后报告清理；输出不确定时先查询，无法确认时通过新认证会话提交原清理事实，让服务端核对原提交。清理确认不能成为重跑模型的许可。新会话只用于历史核对，之后的新任务仍需要新的Assignment/ACK/StartGrant。

新诊断事件cloud_cleanup_confirmed只表示云端核对原CPU执行；output_unconfirmed不宣称输出失败或退款已经完成。显式confirm_cleanup=false只保留旧联调行为，不能宣称云端资源已释放。

验证命令：`python3 scripts/check-cpu-cleanup.py`。协议测试的退出报告是明确夹具，仅证明身份/去重/拒绝与事务行为；另有真实模型、实际Worker进程、可信Counter和真实PG/mTLS验证第二个任务、未交付费用释放、回执失败重启不重跑，以及源删除后保留独立pin直到最终真实文件回收。

仍未完成：公开CPU审核/登记流程、生产价格审批/真实支付、GPU及Windows驱动/多卡、父进程强杀后的未知占用自动恢复、跨进程Broker IPC、全部管理员动作完成聚合与完整UI/云Agent/G4—G6。保留未知状态，不把这些未验证能力标为成功。
