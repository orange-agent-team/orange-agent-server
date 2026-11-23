# Windows x64 设备清单工具

解压后从 PowerShell 运行 `.\bin\orange-worker.exe probe-gpu`。标准输出包含本机 NVIDIA 驱动报告的设备信息；退出码 2 表示此次未取得可用设备清单。程序不安装 WSL，也不申请管理权限。选择是否将该清单交给平台的权利属于设备所有者。[报告字段和缺失读数](docs/nvidia-inventory.md)

本开发包使用固定的 Rust 1.97.1、LLVM-MinGW UCRT 和 Windows x64 LLVM ABI 交叉构建。包内 manifest.json 列出提交、文件摘要和实际导入的系统 DLL。Windows 启动、驱动读取、实机推理与资源回收应分别保存目标机器实测记录；此包只提供设备预检入口。[Rust Windows LLVM 目标](https://doc.rust-lang.org/rustc/platform-support/windows-gnullvm.html)

在目标机运行 `.\Test-WorkerInventory.ps1` 可核对解包后可执行文件的 SHA-256，并读取 Windows 版本、虚拟机监控程序状态、软件包所在卷的可用空间和两个 Windows 可选功能的状态。程序将这些结果与驱动清单一同输出为 JSON。可选功能查不到会独立记录；其开关状态无法证明已有合适的 WSL 发行版。报告供设备所有者保存在本机。[Windows 可选功能状态定义](https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-optionalfeature)
