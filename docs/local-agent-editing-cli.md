# 本地开发 CLI 的显式编辑流程

当前入口为 `cargo run -p orange-agent-runtime --example local-agent -- PRIVATE_CONFIG COMMAND`。编辑后端目前在可用 Apple 平台实测；CLI不是公开服务，不代替桌面OS目录授权，也不会自动批准模型请求。

私有配置沿用只读grant、独立journal、排除目录、模型端点或socket及密钥文件，新增可选 `editing_consent_id`。缺省不提供编辑工具；只读、新建及编辑同意互相独立。模型密钥只在执行step时读取，诊断只输出固定错误类别和已知字段形状。

1. `authorize-edit REQUEST_ID`：明确批准给当前物理工作区编辑权限，返回同意ID和版本；重复请求幂等，已撤销请求不能复活。把返回ID显式写入配置的editing_consent_id。
2. `start`：创建固定能力的Run。旧只读Run不能通过后来加配置或重复start升级权限；可同时配置独立creation_consent_id。
3. `step`：只推进一段模型或已批准工具操作。每个CLI进程都会重新打开Host并恢复确切同意；没有批准不执行。
4. `preview-edit OP_ID`：返回实际文件绑定的diff及preview_digest；不产生文件效果。
5. `approve-edit OP_ID PREVIEW_DIGEST`：单独批准确切预览。参数摘要不等于预览摘要；普通approve也不能批准patch。批准有效期为当前时间加60秒与Run期限的较小值。
6. 再次`step`才执行文件编辑。目标身份、内容或权限变化会拒绝旧批准；未知结果不自动重放。
7. `revoke-edit VERSION`：按确切同意版本撤权；不需要恢复写能力。

`status`只返回run_id、阶段、计数和固定失败原因，不返回pending参数、问题正文、消息或最终文本；不打开工作区、不读取模型密钥，工作区原路径移走后仍可查询。`show`仍要求当前可用读权限和来源校验。

`inspect-edit OP_ID`需要当前读取能力，返回已持久化发布阶段及真实目标/暂存/原件摘要，不返回文件内容，也不恢复写权限。`cancel`沿只读授权停止Run，不会因已撤销的编辑同意而先恢复写权限。独立恢复决策、归档界面和桌面客户端仍待接线，不以这些开发命令声称完整产品已完成。

本机 `scripts/check-file-editing.py` 实际运行CLI子进程、本地HTTP适配器、SQLite和文件：读取后连续编辑/重开无重放、新建再编辑、只读不能升级、撤权后的状态/取消，以及真实写入后回执失败仍unknown。另验证错误批准摘要零批准、native文件名保持、无模型密钥的状态查询和当前目录已移走时元数据查询。HTTP模型与计量响应是明确替身，不是推理认证。

旧创建/问答CLI回归也运行；旧7e115c3 Runtime的schema6已批准创建真实迁移到schema16而不增加批准或重复效果。现有旧vendor目录出现额外.DS_Store时严格清单检查拒绝，验收改用原摘要验证通过的独立归档副本，未放宽校验或改写原vendor。
