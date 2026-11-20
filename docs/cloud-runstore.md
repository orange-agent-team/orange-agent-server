# E07 云端 RunStore 与工作区目录工具

云端把工作区分配、Agent 执行、用户决定、工具派发和结果提交分别持久化。RunStore 只安装在平台 Host 的专用 mTLS 网关。共享 E03 的工具目录、ToolSpec、输入输出校验和 WorkspaceResult 序列化由同一个固定 Rust 依赖提供。云端目录工具将执行环境、批准要求、参数范围和工具版本固定到自己的执行摘要。[Host 协议](../contracts/proto/host.proto) · [用户接口](../contracts/openapi/cloud-agent.v1.json)

## 执行权

平台 Host 通过 Poll 协商输入协议和 RunStore 版本。已有模式可继续处理原有分配；需要新执行权的模型任务只能预留给支持相应版本的 Host。工作区准备记录已绑定输入、CPU 配置及分配标识后，Host 才能调用 RunStore.Claim。该调用按 Run、原 Host、命令和期望版本建立单独的执行世代，绑定用户所有者与根预算。Claim 重试不会延长租约。Renew 校验当前令牌；最近一次续租的重复请求可获得同一已提交状态。每次工具操作还要检查当前有效的工作区、用户、预算、源文件和执行许可。

## 从模型提交到工具结果

1. 用户可在创建模型 Run 时选择允许模型申请列出根目录。服务端返回实际 ToolSpec 派生的模型定义；启用的版本与执行摘要进入不可变输入。单次操作还需单独批准。
2. 推理 Worker 只做审核模型推理。真实模型响应、原始签发版本和计量事实全部提交后，平台 Host 才能获取候选工具操作。首期支持单个 list_directory 调用、根路径点号和 1 至 16 个返回项。模型请求、单纯文本和未经提交的工具参数均不能获得派发许可。
3. AppendIntent 将原响应提交编号、工具版本、参数摘要、用户权限与工作区绑定。用户通过 operations 接口查看审查摘要，再以当前版本批准或拒绝。拒绝停止本次操作。
4. AcquireDispatch 先在 PostgreSQL 记录唯一 permit，再返回一个至多十秒的执行窗口。Host 和 Linux 监督器均先持久记录派发事实，再让固定 Rust 负载执行一次目录枚举。协议没有任意命令或任意文件路径入口。
5. CommitResult 校验执行世代、permit、工作区、内容摘要、受批准的共享结果格式和原始输入版本。成功返回的目录摘要和元数据作为有来源约束的结果保存。请求不确定时，Host 可用 GetOperationReceipt 查询原状态，不能由该查询取得新执行权。回收、撤销或结果许可失效后的未决派发保留 unknown，不自动重复执行。
6. 用户可撤销本次目录权限。后续读取不会再返回被撤销的参数或结果。已经提交的模型推理账务保持实测事实。当前目录工具回合在结果提交后进入 paused 并回收工作区；将该结果送入下一模型回合需要独立的续轮实现。

## 本机验证

```sh
python3 scripts/check-cloud-model-turn.py \
  --database-url-file .local/database-url \
  --identity-config .local/identity/user-config.json \
  --web-repo ../orange-agent-web \
  --model-runtime-root .local/model-runtime \
  --directory-tools
```

工具验收复用整个真实文本推理链，再核对 gVisor 的分配日志、实际目录观察、数据库批准与派发凭据、工作区清理和浏览器可见状态。对 unknown 的故障注入仅在新建的验收数据库中延迟结果记录，使已签发的许可过期。真实身份登录测试、外部 GPU、多轮恢复、扩展工具及生产隔离验证仍各自独立。

目录结果在持久化与传输时使用共享 E03 的带类型信封。用户 Web 只读取经校验的目录数据投影。若提交 RPC 超时或被取消，Host 使用原 permit 查询操作事实，只能重发同一结果提交，不能再把命令送入工作区。逐次批准、拒绝和结果迟到分别以真实虚拟机中的派发标记及控制面中的许可数量核对。
