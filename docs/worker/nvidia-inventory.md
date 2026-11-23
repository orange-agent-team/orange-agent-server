# NVIDIA 设备预检

在贡献者本机运行 `orange-worker probe-gpu`。命令从系统安装位置调用一次 NVIDIA 管理工具，输出一份 JSON 到标准输出。它只读取设备身份、PCI 地址、驱动版本、型号和显存情况。报告留在本机。调度所需的推理实测、隔离证明、用户授权及 Broker 租约各自独立验证。

Windows 检查系统目录以及 64 位 NVIDIA NVSMI 安装目录。Linux 检查系统工具和 WSL 驱动工具目录。按照 UUID 排序可避免驱动重新枚举设备造成顺序变化。工具使用固定的 CSV 查询及 MiB 单位；受限的驱动视图可能缺少 PCI 或显存读数。[NVIDIA System Management Interface](https://docs.nvidia.com/deploy/nvidia-smi/)

协议版本为 1。设备字段 total_mib 与 free_mib 使用十进制字符串，缺少对应测量时为 null。工具成功报告设备时状态为 observed。观测时的空闲显存随其他进程变化，不能作为下一次分配容量的承诺。若没有可用设备、平台不支持、工具不存在、驱动报错、查询超时、清理未确认或输出无效，命令以退出码 2 返回对应状态及空设备列表。在具有受支持驱动的主机上仍需实际加载审核模型、确认停止并检查显存回收，才能记录该组合的硬件认证。
