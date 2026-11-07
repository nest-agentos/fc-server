# 通用服务与持久根盘

管理服务启动只提供 API。普通 `start` 创建 VM；服务管理层按管理员安装的服务包，通过现有 guest SSH 准备文件、运行进程、检查就绪并发布端口。guest 镜像不包含业务服务定义，核心没有按应用名分支。

## 查看与连接虚拟机

```bash
./target/release/firecracker-server list
./target/release/firecracker-server list --all
./target/release/firecracker-server list --json
# Equivalent source entry point:
./vmm.sh list
# Connect to a VM as agent:
./vmm.sh ssh VM_ID
```

`list` 默认显示当前 VMM 实例（包括启动/停止中的实例），列出 VM ID、profile、服务、VM/服务状态、guest IP、根盘是否持久及端口映射；普通 VM 也会显示。`--all` 额外包含已停止的服务绑定和保留的根盘；不包含未完成的初始化目录。`--json` 输出 `{"vms":[...]}`，诊断日志写入 stderr。列表使用一次管理 API 快照，端口随 VM 信息返回，不逐台请求。

管理配置和 API key 是 root 私有文件，普通用户运行二进制的 `list` 命令时会通过 sudo 调用已安装的可信列表脚本；不需要手工读取或导出密钥。查询失败会明确报错，不会冒充空列表。服务 `ready` 还需核对当前监督状态及 VM 实例身份，`vmm-running` 本身不代表应用就绪。

## 更新管理程序，保留已有镜像

确认没有活动 VM，再更新宿主程序：

```bash
sudo python3 scripts/network-environment.py idle
sudo systemctl stop firecracker-server
./vmm.sh install-server
sudo systemctl start firecracker-server
```

`install-server` 只构建/安装 Rust 管理程序、Python 宿主辅助脚本及 systemd unit，不构建或替换内核、Debian 镜像、Firecracker、virtiofsd、管理配置或 API key。首次环境仍使用 `setup`、`network`。现有 guest 必须已有 Python 3、SSH、公钥和应用运行依赖。

## 配置持久运行环境

在管理员选定的 profile 中加入：

```toml
[profiles.services]
# Also supply the existing kernel, rootfs, resource limits and network_slots.
rootfs_persistent = true
max_lifetime_secs = 0
```

该片段不是完整 profile；可以复制已有 `debian13` profile 后添加这两个字段。网络继续使用已有的宿主槽位和 guest DHCP，目录绑定使用既有授权。`0` 明确表示由管理员管理生命周期，VMM/backend 退出仍会触发回收。其他 profile 的原有有限生命周期保持不变。

默认 `rootfs_persistent = false`。启用后，可写根盘按 **VM ID** 保存在 `state_dir/roots/VM_ID/`：

| 模式 | 持久保存内容 |
| --- | --- |
| private | 完整可写根盘、首次创建时的内核 |
| shared | 可写 ext4 upper、首次使用的只读基础镜像硬链接、内核和 initrd |

guest 的 `/home/agent`、`/etc` 和已安装软件等根盘修改会保留。`/run`、`/tmp` 等内存文件系统以及进程内存不属于磁盘持久化；再次启动是重新引导，不是内存快照恢复。直通目录和额外挂载卷继续遵守各自的持久化规则。

共享模式必须将 upper 与原 lower 配套保存，禁止把旧 upper 自动套到更新后的 Debian 基础镜像上。更新全局镜像仅影响新建根盘；已有持久根盘继续使用固定的启动制品。若要采用新基础镜像，应备份数据后新建 VM/迁移数据。不能原地改写共享基础镜像 inode。

停止、到期、管理服务正常重启和 guest reboot 都只删除临时 jail/cgroup，不删除持久根盘。再次使用同 ID、相同存储配置创建 VM 时复用该磁盘；profile、卷集合、目录集合或绑定配置不兼容时明确拒绝。磁盘文件通过硬链接接入 jail，不做每次启动的全盘复制。

首次创建使用临时目录和完成清单提交；失败初始化会回滚。管理进程在初始化期间被强杀时可能留下 `VM_ID.initializing`，须确认无关联 VMM 后核对该未完成目录。异常退出的旧 jail/cgroup 仍按 README 的既有流程处理，不自动接管未知进程。ext4 提供崩溃恢复基础，强停不承诺尚未落盘的应用数据。

## 安装与运行服务

示例包见 [python-web](../examples/python-web/service.toml)，所有字段由通用服务层解释：

```bash
sudo /usr/local/bin/firecracker-server service install \
  "$PWD/examples/python-web/service.toml" \
  --identity "$HOME/.ssh/mindos-firecracker"

# Prepare files and leave the VM ready for an explicit start.
sudo /usr/local/bin/firecracker-server service setup web-demo python-web
sudo /usr/local/bin/firecracker-server service start web-demo

# Or prepare, start and enable host boot restoration together.
sudo /usr/local/bin/firecracker-server service up web-demo python-web --enable
sudo /usr/local/bin/firecracker-server service status web-demo
curl http://127.0.0.1:18080
```

源码调用可替换为 `./vmm.sh service ...`。服务 ID 与 VM ID 各为 1–48 个 ASCII 字母、数字或 `-`。一台受管 VM 对应一个监督入口；该入口可自行组织多个应用进程，或用同一镜像创建不同服务 VM。

| 定义字段 | 含义 |
| --- | --- |
| `id` / `profile` | 服务 ID 与已授权 VM profile |
| `directories` / `volumes` | 授权直通绑定和已经创建的持久卷 ID，不自动导入卷 |
| `command` | guest argv 数组，不经过隐式 shell |
| `working_directory` | guest 绝对工作目录 |
| `environment` | guest 环境变量表，随私有服务包保存 |
| `files` | 包内相对 source、guest 服务目录下相对 target，可选 executable |
| `ready_port` | guest loopback TCP 就绪端口；启动前拒绝已有监听者 |
| `startup_timeout_secs` | SSH/应用就绪等待上限，1–300 秒，默认 60 |
| `publish` | 通过现有 API 发布的 TCP/UDP 映射，只在服务就绪后发布 |

`{service_dir}` 可用于命令参数与环境变量，展开为 `/home/agent/.local/share/firecracker-services/SERVICE_ID`。单个包最多 64 个文件、总计 32 MiB；更大的程序可通过授权直通目录读取。配置和小脚本每次启动重新投递；其他用户文件保留，不执行目录镜像删除。

安装将文件与 SSH 私钥快照保存到宿主 `/var/lib/mindos-firecracker/services/packages/`，权限由私有根目录和 0600 文件约束；后续启动不依赖原始服务包或用户的 SSH 私钥路径。同 ID 同内容重复安装幂等；更新包要求所有绑定 VM 的监督服务均停止。profile、挂载等存储变更仍受持久根盘兼容检查限制。服务包和业务凭据不应提交到仓库。

宿主的 `firecracker-workload@VM_ID.service` 监督 SSH，guest 辅助进程监督应用进程组。SSH 控制通道 EOF 会终止应用；失败会清理当前 VM 并由 systemd 重试。就绪事件通过 `sd_notify` 上报，端口全部发布成功后才算启动完成。服务退出、SSH 失败、就绪超时和端口冲突均记录分类日志，原始应用输出不写入管理日志。

管理服务正常重启会先停止关联监督 unit，再重建 VM、准备文件、启动服务并恢复端口。`--enable` 控制宿主开机恢复；不加时仍会在本次运行期间监督服务。服务清理按 VM 实例标识核对，拒绝删除同 ID 的替代实例。

## 停止、销毁与日志

```bash
# Disable boot restoration and stop the VM; persistent root and volumes remain.
sudo /usr/local/bin/firecracker-server service stop web-demo

# Restore the same root disk and service later.
sudo /usr/local/bin/firecracker-server service start web-demo

# Explicitly delete the root disk and VM-to-service binding; extra volumes remain.
sudo /usr/local/bin/firecracker-server service destroy web-demo
```

底层 `DELETE /v1/vms/VM_ID` 仍表示停止与回收当前 VM 实例，保留持久根盘。新增 `DELETE /v1/roots/VM_ID` 显式删除已停止 VM 的持久根盘；活动 VM、待完成操作及旧 jail/cgroup 存在时拒绝删除。两者均要求管理 API 认证。受管服务运行时直接停止底层 VM 会触发监督重建，正常运维使用服务命令。

宿主日志：`journalctl -u firecracker-workload@VM_ID`。guest 私有应用输出：`SERVICE_DIR/service.log` 和 `.1`，每个最多约 5 MiB；应用自己创建的日志遵循应用配置。查看输出前注意其中可能包含业务凭据。诊断日志只记录操作 ID、阶段、分类、耗时和非敏感数量，不记录完整 argv、环境或文件内容。

服务定义由宿主管理员安装，SSH 用户固定为基础镜像中的非 root `agent`。它是同一管理员信任域的部署机制，不提供服务包之间的租户隔离。就绪目前是 TCP 探测，完整业务功能仍需调用应用接口验收。

回归：`python3 scripts/test-services.py`、`cargo test --locked`，其中涉及测试进程的检查应放在仓库 temporary-process 有界作用域内执行。
