# Windows x64 设备清单工具

解压后从 PowerShell 运行 `.\bin\orange-worker.exe probe-gpu`。标准输出包含本机 NVIDIA 驱动报告的设备信息；退出码 2 表示此次未取得可用设备清单。程序不安装 WSL，也不申请管理权限。选择是否将该清单交给平台的权利属于设备所有者。[报告字段和缺失读数](docs/nvidia-inventory.md)

本开发包使用固定的 Rust 1.97.1、LLVM-MinGW UCRT 和 Windows x64 LLVM ABI 交叉构建。包内 manifest.json 列出提交、文件摘要和实际导入的系统 DLL。Windows 启动、驱动读取、实机推理与资源回收应分别保存目标机器实测记录；此包只提供设备预检入口。[Rust Windows LLVM 目标](https://doc.rust-lang.org/rustc/platform-support/windows-gnullvm.html)

在目标机运行 `.\Test-WorkerInventory.ps1` 可核对解包后可执行文件的 SHA-256，并读取 Windows 版本、虚拟机监控程序状态、软件包所在卷的可用空间和两个 Windows 可选功能的状态。程序将这些结果与驱动清单一同输出为 JSON。可选功能查不到会独立记录；其开关状态无法证明已有合适的 WSL 发行版。报告供设备所有者保存在本机。[Windows 可选功能状态定义](https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-optionalfeature)

仅使用远程算力时，可以用 `Test-WorkerInventory.ps1 -Mode Consume` 输出本地需求说明。消费模式可单独运行此脚本，无需提供 Worker 程序或发行包。此分支在读取 Windows 可选功能、GPU 或 WSL 注册情况之前结束。贡献模式使用 `-Mode Contribute`，也可用 `-RequiredPackageVolumeBytes` 指定这次安装实际需要的包目录卷空间。省略时磁盘余量照常记录，容量比较为 null。

贡献模式额外运行 `orange-worker.exe probe-wsl`。它对照当前用户的名称和详细列表，分别统计 WSL 1、WSL 2 的注册数量。汇总不含发行版名称、默认用户或主目录。查询失败、两次列表不一致或结果无法识别时保留原因。已有注册数量不能证明该环境属于平台或能够安全执行任务。报告的 GPU 退出码仍仅表示是否取得设备清单。[微软 WSL 列表命令](https://learn.microsoft.com/en-us/windows/wsl/basic-commands)
