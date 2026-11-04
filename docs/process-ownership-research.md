# 崩溃后进程所有权：当前调研与实测

2026-11-05。用途是补充模型监督器的OS后端，不把进程编号当成可恢复的终止权限。

## 研发选择与来源

Apple公开指南将用户后台任务交给launchd管理，并要求被管理程序保持受管理形态、不要自行脱离会话。这支持我们优先使用系统作业身份的方向；它本身不证明Orange应用的签名、App Sandbox或安装流程已通过。[Apple进程管理指南](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)

本机`/usr/share/man/man5/launchd.plist.5`说明AbandonProcessGroup关闭时，作业退出会清理同进程组中的剩余进程。实验显式使用false、KeepAlive=false、三秒ExitTimeOut；只注册当前用户gui域的随机唯一label，plist保留在工作区，不写任何LaunchAgents/LaunchDaemons安装目录。没有采用会在失败后保活的旧`launchctl submit`方式。

XNU源码存在使用uniqueid/idversion校验进程身份的内核查找；相关结构也有PRIVATE边界。因此我们不把这些内核接口视作已验证的稳定应用API。研发决策是：恢复时优先依靠公开系统作业/容器的所有权，不对状态文件中的旧PID直接发信号。[Apple XNU进程身份实现](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_proc.c)、[Apple进程信息头文件](https://github.com/apple/darwin-xnu/blob/main/bsd/sys/proc_info.h)

Linux的pidfd提供以文件描述符指定进程的发送信号接口，可作为另一个平台方向；当前尚未完成Orange的Linux目标验证，也没有据此新增或浮动升级依赖。[rustix公开接口](https://docs.rs/rustix/latest/rustix/process/fn.pidfd_send_signal.html)

## macOS实验实际做了什么

`python3 scripts/probe-launchd-cleanup.py`生成私有临时配置，注册一个限时用户作业。其父进程创建子进程并传递已锁定的文件描述符；子进程确认继承锁、记录文件dev/inode与nonce摘要。控制器核对同一进程组、同一子进程、同一租约文件和nonce，再要求测试父进程自行SIGKILL。控制器从不对文件记录的PID发送信号。

在子进程自身25秒期限之前，测试要求租约五秒内重新可获得，随后移除自己成功注册的唯一作业并复查。父/子都有期限；错误时记录失败，不把权限错误或超时当完成。报告保留在`.local/launchd-probes/<id>/report.json`；正式执行日志在工作区执行台账的ENV-MAC-PROCESS证据中。

首次实验观察到约0.04秒锁释放并成功移除注册；后续增强了子进程锁及dev/inode校验，结果以对应脚本SHA和台账日志为准。单次观察不是延迟SLA，也不表示任意程序都无法脱离作业进程组。

## 已实现与尚未实现

模型宿主新增可选私有控制nonce：有该配置时，必须在五秒内收到正确父端认证帧，之后才可打开模型/执行租约或启动子进程。继承匿名管道旧模式保持兼容。错误或缺少认证不会创建模型状态、API密钥或执行租约。配置读取同时核对打开后的文件身份、类型与私有权限。

尚未完成Rust的launchd模型适配器、打包后的SMAppService、Flutter App Sandbox接入、签名/公证、真实模型权重、GPU清理和任意孤儿进程恢复。普通监督器崩溃后的旧记录仍保守隔离。下一步需要把私有控制通道、服务唯一身份、确认退出和恢复记录串成可运行适配器，再做真实进程故障测试；不能仅凭这份实验删除unknown或将资源置为idle。
