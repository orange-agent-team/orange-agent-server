# 保留版本导出：实现检查点

当前已实现Runtime导出读取、格式校验和持久发布，尚未接入桌面目的目录选择及导出按钮。主线仍为schema17的保留管理；本候选使用schema18，未合并。不等于完整用户数据退出功能，删除未实现。

## 已实现

`capture_retention_export(grant, choice)` 在独立PatchRetentionGrant下读取所选Original、Staged或Both版本，按当前实际文件观察生成 `orange-retained-files.v1` 数据包。每个源文件上限1MiB，总编码包上限3MiB；缺失的单选版本或完全无文件拒绝。拒绝软链接、硬链接、非普通文件、文件身份/权限/内容在读取时变化，以及不匹配的目录身份。

数据包包含原操作ID、逻辑相对路径、选择、当初审阅的原始/结果摘要及当前所选版本的实际字节（base64）、长度、内容和安全元数据摘要、原inode/内容匹配标记。没有绝对目录、私有Journal路径、受保护暂存名、授权token或可执行导入指令。base64是可直接解码的文件内容，不是加密。

`RetentionExportCapture` 不可序列化/Debug；preview仅返回数据包摘要和大小等元数据。`encoded_bytes`供后续可信发布器使用，本身不授予写入或删除权限。它持有真实读取租约，撤权后两种访问均失败，原生权限释放必须等待capture销毁。普通读取错误不误撤权；超时或未完成任务仍按CancellationGuard撤权。

`verify_retention_export`是离线格式/内部完整性校验，不产生文件权限或导入动作。它检查版本、未知字段、相对路径、选择与唯一角色、长度、base64内容摘要和内容匹配标记；返回实际包长度及摘要。校验和无密钥，不证明生产者身份、外部元数据声明或来源可信。

## 已实现的目的能力与持久发布

`authorize_retention_export_destination(source, root)` 创建独立目标能力，绑定拥有者、原操作、源选择随机环境和Host epoch，排除全部私有根；不是从普通只读或编辑能力转换。目标必须具备持久物理身份。

`prepare_retention_export(source, destination, choice, file_name, request_key)` 返回不可序列化的PreparedRetentionExport。其预览绑定实际数据包摘要/大小、单一目标文件名（必须以.orange-retained.json结尾）、目标物理目录和独立选择、有效期及唯一暂存名。预览不写文件；原件内容仅保存在受撤权约束的内存capture，不复制进Journal。每个拥有者最多1000条导出审计记录，达到限制拒绝，不自动删除。准备结果持有源和目标读取租约。

`approve_retention_export` 消耗prepare结果；精确摘要、当前两个许可及有效期通过后，在SQLite事务中先将prepared变为dispatching。此后才新建0600、非执行文件，写入及同步、核对实际字节和inode，再以NOREPLACE排他发布，核对最终文件并同步目录后记录成功回执。已有目标（含晚到目标）不覆盖，原件与原任务/归档额度不变。重放已派发记录被拒绝。

schema18的patch_exports绑定与终态不可改写/删除；重开将dispatching转为unknown并记录事件。派发后的失败、回执失败或撤权可能留下暂存或最终数据包，保守标unknown，不能删除或重发来掩盖结果。`retention_export_outcome(owner,run,operation,export_id)` 只核对并返回状态及review摘要，不读目标文件或恢复权限；成功回执先核对格式、记录摘要和数据包元数据。存在文件不等于导出成功。

本实现验证发布时观察到的文件，不保证外部程序此后不能修改或替换目标，也不声称全局原子命名空间内容CAS。

## 下一步必须实现

1. 固定共享包导出、消费者显式导入及匹配的模型组件；不手改vendor或隐式使用相邻源码。
2. Desktop独立native目标选择、清晰展示目的目录/文件名/包含的内容/大小/摘要，一次明确确认及丢失回复查询。源保留token不能当作目标token。
3. Host持有PreparedRetentionExport时，它同时持有源和目标租约；导航、撤权或关闭时先丢弃未派发预览，再等待排空，最后释放native scope。派发中不销毁仍在使用的能力；即时撤权信号与Actor队列保持分离。
4. 实际macOS/FRB集成覆盖真实导出、离线校验、源不变、目标冲突、读写中断、重复确认、回执失败及重开。确认实际App Sandbox目标标签/权限行为，不能用普通Rust测试代替签名应用验证。结束后恢复默认App。
5. 仍须处理unknown导出的位置检查和明确清理；当前不自动清理未知暂存文件。删除原件/释放归档额度另行设计精确授权与持久回收记录。成功导出不等于删除授权。

## 证据与限制

196项Runtime测试通过（retention-export-runtime-full.log）；修正两个Clippy嵌套分支后严格Clippy及相关2项真实文件测试通过（retention-export-clippy-fixed.log）。负例覆盖缺失暂存不误撤权、二进制实际内容、修改后不伪装原始内容、链接拒绝、跨scope/epoch、capture阻止提前排空，以及9类数据包篡改拒绝。第一次普通错误误撤权失败保留于retention-export-capture-tests.log，修复后原断言未变。

已有普通Rust进程下的实际目标写入验证；尚无签名桌面端的实际目标选择/写入验收，不能据此声称用户已经能从产品完成导出，也不登记完整数据退出任务DONE。

Runtime发布新增4项实际文件测试（retention-export-inflight-tests-fixed.log）：排他新建、0600权限、精确摘要确认、重复批准拒绝、源/目标预先及暂存后撤权、晚到目标、派发事务失败、实际写入后回执失败、重开unknown不重放及schema17形状迁移。派发中撤权钩子仅cfg(test)编译，正式API没有故障注入方法。

真实固定a2cdafb/schema17旧CLI先成功编辑、再批准第二次编辑；新schema18继续执行并保留批准、版本边及旧捕获证据，旧CLI再打开明确拒绝且DB/WAL摘要不变（retention-export-schema17-compatibility.log，72356 exit0）。199项Runtime全量测试曾通过；随后增加第200项暂存后撤权用例，相关4项及严格Clippy通过。共享发布前再执行全部共享测试。

## 重开后的记录发现

retention_export_outcomes(owner,run,operation,before,limit)返回1—100条已核对元数据，按不可变export ID倒序分页（不是创建时间排序），不返回文件名、路径、内容或任何文件能力。跨owner/Run/operation、无效游标及页大小拒绝；每条记录复用已有回执完整性验证。准备中和已成功记录在撤权及Host重开后仍能由拥有者发现。相关实际文件/分页/重开负例及严格Clippy通过，retention-export-index-tests.log。
