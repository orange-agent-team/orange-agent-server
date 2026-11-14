# 未确认导出的独立检查

本增量只有目的位置的只读检查，不清理、不复制、不发布，也不把unknown补成成功；原件处置仍未实现。它保持schema18，不改写已有导出记录。实现与桌面消费者的独立native检查流程一同验收。

## Runtime接口

`authorize_export_inspection(owner, run, operation, export_id, root)` 在用户重新明确选择对应导出目录后创建不透明ExportInspectionGrant。它绑定当前Host epoch、拥有者、原Run/operation、导出ID和不可变review摘要，并核对目标物理目录身份。原目录可以已撤权或不可用；新能力不能转为源目录读取、普通Agent能力或写入能力。仍排除全部私有根。

`inspect_export_files(grant)` 只按已核对的持久记录读取目标名称及该次唯一暂存名，不扫描整个目录。两个名称均严格验证；每个普通单链接文件最多读取3MiB，拒绝符号链接、硬链接、设备/目录及超限数据。文件描述符、名称绑定、内容长度、权限摘要和对象身份在读取前后复核。

返回ExportInspectionReceipt及metadata view：导出ID、review摘要、记录状态、用户目标文件名、目标/暂存的存在性、长度、SHA、权限/对象摘要、是否匹配原批准的数据包、是否为格式有效的数据包。没有原目录路径、原件内容、原始文件摘要、base64数据或内部暂存名。`matches_approved_artifact`仅证明当前观察内容与批准的包一致，不证明缺失的完成回执。

Receipt持有真实读租约；撤权后view访问失败，wait_drained需等receipt销毁。Host重开后旧grant不能再检查，新选择可继续查看未知、已完成或未派发记录。所有检查都保留原record状态；文件后来改变也不删除历史成功回执。

## 已有证据

export-inspection-runtime-tests.log：5个导出相关测试（含多个情形）及严格Clippy通过。实际写入后回执失败时目标包匹配但record仍unknown；晚到外部目标不匹配、暂存匹配；派发前拒绝时两个位置均缺失。错owner/Run/目录/私有根、撤权与排空、重开epoch、链接与超限拒绝均覆盖。检查不能恢复源权限，序列化投影不含源文件名、内容或暂存名。

## 下一步

1. 固定共享源发布/消费，并在Desktop实现与源保留会话独立的native导出检查选择。入口从已有导出记录列表选择ID，不需要重新授权原目录。
2. 检查token不能复用导出目的、源读取或恢复写入token。读取结果只在当前会话/页面版本有效；关闭/Host停止先信号撤权，再排空，最后native release。无源scope也应能关闭此独立检查。
3. 页面清楚区分“内容匹配”与“导出回执已确认”。即使目标存在且匹配，也不能自动写成功回执、重发或删除。实际签名macOS文件/FRB集成验收后恢复默认App。
4. 清理是后续独立写入能力和精确批准流程。必须先处理旧导出worker是否仍可能写入的证据、目标对象/命名空间并发变化、派发前持久记录及失败后保留状态；检查快照本身不是删除授权或证明writer已停止。当前不提供清理动作，也不释放归档额度。

## 桌面消费证据

Desktop已接入独立目的检查Host/signals、只读FRB投影及导出记录列表入口。12Rust/Clippy/60组件测试、实际签名macOS导出/独立检查，以及原保留1/恢复4原生回归通过。93837实际场景将自有源目录移走，关闭原源与目的写授权，再从持久记录选择目标读取；目标匹配但unknown保持unknown，源2份及导出2份均保留，关闭后API拒绝读取。模型和系统选择器回调是替身，人工选择器和其他OS不由此认证。见export-inspection-native-acceptance.log及consumer scripts/check-export-inspection.py。

上述下一步中的消费、只读页面与原生检查现已实现；后续剩余为明确处置所需的旧writer终止/对象和命名空间证据、独立写授权及不可重放的持久清理流程。没有根据匹配快照自动确认或删除。
