# 本地 Agent 的模板选择与运行绑定

此版本提供四个可选择的内置模板。代码审阅的模型配置及来源要求见下文。**文本摘要（AU-02）**仅把用户提交的文本交给本地模型，模型请求中的工具目录为空。**授权文件问答（AU-01）**可请求 read_file 和 search_text，文件访问仍需当前工作区授权及正常的操作批准。已提交的读取结果给出相对路径、SHA-256、字节起止范围，以及在搜索命中时可用的行号。没有成功读取任何来源时，该模板以 skill_source_required 结束。

模板版本记录在 crates/agent-runtime/src/skills 下的发布目录中。正文、显示名、版本、场景和工具声明共同参与摘要。每个所有者分别启用指定摘要，模板变更不会扩大工作区能力。本节说明随 Runtime 分发的只读模板。本地 JSON 包见下文。[模板接口](../contracts/json-schema/skill-template.v1.json)

## 本地 CLI

在现有 0600 私有配置中保留 owner、run_id、工作区、本地模型和预算设置。运行以下命令查看模板正文、工具和版本，再将选定条目的 pin 对象填入配置的 skill 字段。revision 为该所有者对该模板的当前启用版本，初始为 0。

```sh
local-agent PRIVATE_CONFIG skills
local-agent PRIVATE_CONFIG enable-skill 0
local-agent PRIVATE_CONFIG start
local-agent PRIVATE_CONFIG step
local-agent PRIVATE_CONFIG skill-binding
```

问答提出读取请求时，沿用 local-agent 的正常查看和批准流程。完成后的授权视图带有 source_references。它们来自宿主持有的真实工具回执；单纯在模型文本中出现的路径不会产生引用。恢复前宿主重新核对工作区及来源内容。摘要的用户文本和模板指令分别作为用户消息和系统消息保存。

使用 `local-agent PRIVATE_CONFIG disable-skill REVISION` 停用。再次启用会取得新的修订号。旧 Run 保留原模板和执行记录，旧修订的待执行步骤在下一次推进时取消。新模板或新的启用修订需新建 Run。SQLite schema 30 为既有 Run 保留其原配置及消息；升级不向既有 Run 绑定模板。删除配置中的 skill 字段仍可使用原有通用 Agent 入口。

## 验收

`python3 scripts/check-real-agent.py --verify-skills` 使用固定的本地 CPU 模型，核对摘要中的日期与数量，然后通过独立 CLI 进程批准实际文件读取，并比较最终引用与授权文件的真实摘要。配置已准备的模型缓存可通过 --model-runtime-root 指定。另运行 `python3 scripts/check-skill-journal-upgrade.py`，它用固定的 schema 29 源码建立原始运行记录、升级并重新用旧程序打开，以检验拒绝行为和数据保留。

## 用户安装的本地模板

本地 CLI 接受 UTF-8 JSON 清单，命名空间为 `local.`。单份清单上限为 16 KiB。文件的字段和工具枚举见模板接口。安装前显示完整指令、声明工具和规范化内容的 SHA-256。安装和启用分别确认，当前版本只支持声明式指令和宿主已提供的只读工具。允许的文件读取仍沿用每次真实操作的授权。AU-01 模板须取得实际读取或文本搜索的引用才能完成。

```json
{"id":"local.delivery-report","version":"1.0.0","name":"交货摘要","scenario":"AU-02","instruction":"用一句话概括交货情况，保留原文中的日期和数量。","tools":[]}
```

```sh
local-agent PRIVATE_CONFIG review-skill < manifest.json
local-agent PRIVATE_CONFIG install-skill REVIEWED_DIGEST INSTALLATION_REVISION < manifest.json
local-agent PRIVATE_CONFIG skills
local-agent PRIVATE_CONFIG enable-skill ACTIVATION_REVISION
local-agent PRIVATE_CONFIG start
local-agent PRIVATE_CONFIG step
local-agent PRIVATE_CONFIG uninstall-skill INSTALLATION_REVISION ACTIVATION_REVISION
```

将审核结果的 pin 填入配置的 skill。可同时填写整数 skill_activation_revision，启动事务会再次核对用户选择时的启用修订。安装修订按所有者、标识及版本递增，启用修订按所有者及标识递增。每个用户可以独立安装同名同版本的模板，每个用户的已审核版本只能对应一个摘要。可同时安装多个版本，显式启用旧版本即可回退。切换版本或卸载当前版本会撤销旧 Run 的续跑权限。卸载后，已有 Run 可查询其原始绑定；运行从未引用过的模板正文会清除，已用版本保留历史快照。再次安装仍需匹配原摘要。SQLite schema 31 兼容原通用 Run 和内置模板绑定。CLI 验收可运行 `python3 scripts/check-real-agent.py --verify-local-package`。

## 本地代码审阅（AU-06）

在已经选择的本地仓库中启用“代码审阅报告”，在任务中指定需要检查的相对文件和预期行为。模板可申请文件读取或文本搜索。用户批准读取后，模型报告问题、影响和修改建议，宿主附上实际读取版本的路径、字节区间和 SHA-256。缺少已批准的来源时，Run 以 skill_source_required 结束。审阅工具目录不含执行、提交或推送操作。

使用合格的模型配置运行 `python3 scripts/check-real-agent.py --verify-code-review --review-model-7b MODEL_FILE` 会创建临时 Git 仓库，使用固定的真实 CPU 模型诊断样本计算错误，核对引用、回读报告、文件摘要和 Git 工作区状态。

审阅的模型资格按权重单独记录。可通过 `--review-model-7b /path/to/qwen2.5-7b-instruct-q4_k_m-merged.gguf` 明确选用已合并的固定 7B 权重。验收会核对文件长度与完整 SHA-256，然后使用与本地目录隔离的 Unix socket 启动模型。0.5B 测试权重在早期验收中曾读取源码后误判计算逻辑。当前官方模板要求显式选择经锁定的 7B 配置；普通 0.5B 配置在创建 Run 前拒绝审阅。日志格式 32 会阻止旧宿主绕过新增的引用和模型门槛。[7B 权重来源与锁定摘要](../config/local-code-review-model.lock.json)

## 批准后应用补丁（AU-07）

macOS 本地 CLI 的“批准后应用补丁”用于修改现有文件。该官方模板需要独立的目录编辑授权、读工作区、已启用的准确版本及匹配的模型配置。它可请求 read_file 和 apply_patch。请先批准读取，然后用 `preview-edit OPERATION_ID` 查看原摘要、差异和结果摘要。核对后，用 `approve-edit OPERATION_ID PREVIEW_DIGEST` 批准这个预览，再继续 `step`。宿主保留原始文件和提交回执。

创建 Run 时只提供读取权会被拒绝。补丁必须引用同一 Run 已批准读取的文件版本。没有成功的补丁回执时模型不能报告模板已完成。停用再启用后，旧 Run 已有的编辑批准也无法派发。用户安装的 local.* 清单仍只支持声明式只读工具。

补丁模板使用 schema 33 阻止不识别其编辑约束的旧宿主修改日志。实际文件系统的资格以已验证的目标平台为准。

本地 CPU 模型宿主接受受信启动配置中的可选整数 `inference_threads`（1 至 16）。默认值为 2。显式分配会在宿主就绪回执中给出实际启动参数的线程数与执行配置摘要。AU-07 本地组合验收使用 4 个推理线程，单次推理仍受 120 秒上限及原 Run 截止时间约束。Worker 与计量服务沿用已锁定的默认配置。

## 本地宿主的模型身份

桌面等宿主可以将已鉴别的模型别名、上下文长度、权重摘要和后端程序摘要一起绑定到 Run。桌面的历史连接标识仍取原有四项的规范化摘要。代码审阅和补丁模板使用已锁定的 7B 权重及其 8192 token 配置。可信宿主通过 `start_agent_skill_on_local_model` 传入实际连接和当前工作区权限。

本地 CLI 在推进这两种模板时会读取原 Run 的绑定，并检查私有模型服务的 profile.json、process.json、Unix socket、会话密钥位置和运行实例。编辑配置文件中的模板或仅改变模型别名无法替换原 Run 的权重约束。可用 `python3 scripts/check-real-agent.py --verify-weight-admission` 验证真实 0.5B 权重被拒时没有模型调用。合并后仅供本地导入的权重可通过 ModelCache 的 `ArtifactSpec::local_only` 校验，导入来源是用户明确选择的文件。

## 模型派发中断的核对

本地 CLI 的 `model-observation` 使用配置中的原所有者和 Run 编号，读取最近一次已观测到的未确认模型派发。返回调用编号、观测时间及原因。运行时先到达单次等待上限为 `call_time_limit`，模型适配器先返回失败为 `gateway_failed`，整个 Run 的截止时间到达为 `run_deadline`。这三类还返回当时设置的等待上限和实际单调时钟耗时（毫秒）。宿主重新打开日志时发现先前仍处于已派发状态，会记录 `host_reopened`，其耗时未知。

诊断与相应恢复状态在同一 SQLite 事务中提交。记录仅对仍获授权的原所有者开放，不含消息正文、路径、密钥或模型回复。既有 `model_unknown` 和不重放约束继续生效。schema 34 对已经标为未知的历史行保留原记录，不推断事后原因。

schema 35 起，支持分类的适配器可以在 `gateway_failed` 观测中额外给出 `adapter_code`。例如 `unavailable` 表示该适配器未取得可用响应，`protocol` 表示响应违反协议，`limit` 表示超出受控边界，`incomplete` 表示未得到完整回复。它们描述适配器观察到的现象，不代表底层进程的最终执行结论。旧适配器继续工作并返回 `unclassified`，已存储的历史观测不会在升级时获得推测出的类别。
