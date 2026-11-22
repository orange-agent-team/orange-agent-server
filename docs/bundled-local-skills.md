# 本地 Agent 的模板选择与运行绑定

此版本提供两个可选择的内置模板。**文本摘要（AU-02）**仅把用户提交的文本交给本地模型，模型请求中的工具目录为空。**授权文件问答（AU-01）**可请求 read_file 和 search_text，文件访问仍需当前工作区授权及正常的操作批准。已提交的读取结果给出相对路径、SHA-256、字节起止范围，以及在搜索命中时可用的行号。没有成功读取任何来源时，该模板以 skill_source_required 结束。

模板版本记录在 crates/agent-runtime/src/skills 下的发布目录中。正文、显示名、版本、场景和工具声明共同参与摘要。每个所有者分别启用指定摘要，模板变更不会扩大工作区能力。当前接口面向随 Runtime 一起分发的只读模板。其他目录、第三方包安装和写操作模板仍有各自的实现与认证要求。[模板接口](../contracts/json-schema/skill-template.v1.json)

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
