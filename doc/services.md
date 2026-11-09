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

管理配置和 API key 是 root 私有文件，普通用户运行二进制的 `list` 命令时会通过 sudo 调用已安装的可信 Rust 管理程序；不需要手工读取或导出密钥。查询失败会明确报错，不会冒充空列表。服务 `ready` 还需核对当前监督状态及 VM 实例身份，`vmm-running` 本身不代表应用就绪。

## 更新管理程序，保留已有镜像

确认没有活动 VM，再更新宿主程序：

```bash
sudo /usr/local/bin/firecracker-server network idle
sudo systemctl stop firecracker-server
./vmm.sh install-server
sudo systemctl start firecracker-server
```

`install-server` 构建/安装 Rust 管理程序、静态链接的 Rust guest agent 及 systemd unit，不构建或替换内核、Debian 镜像、Firecracker、virtiofsd 或 API key。升级时自动迁移标准 helper 路径和网络 unit，原配置备份为 `config.before-rust-runtime.toml`，并删除旧运行期 Python helper。首次环境使用 `setup --os`、`network --os`。日常 `build` 默认也跳过 OS；仅 `build --os`（或初始化命令显式传入 `--os`）处理 kernel/image。宿主运行期及 guest 监督进程不依赖 Python；静态 guest agent 通过 SSH 自动上传至 agent 用户目录。现有 guest 只需 SSH、公钥和应用自身的运行依赖。构建、安装和验收脚本仍有 Python 3.11+ 依赖。旧版本已启用网络的安装，在停止管理服务后更新程序，再重启 `mindos-firecracker-network`，最后启动管理服务，以建立新增的代理规则集合。

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

## 实例 CPU 与内存

实例可在私有 YAML 中声明部分或全部资源字段：

```yaml
vm:
  id: cell-x1
  profile: services
  resources:
    memory_mib: 4096
```

省略的字段继承 profile 默认值。服务 `service.toml` 也支持 `[resources]` 表；YAML 存在 `vm.resources` 时使用该表替换 manifest 的资源请求，其余字段继承 profile。直接创建 VM 的 API 接受同样的 `resources: {vcpus, memory_mib}` 对象。

管理员在已有 profile 中设置 `max_vcpus` / `max_memory_mib` 上限；省略时上限等于该 profile 的默认值，保持旧授权边界。默认值不随实例请求修改。例如，`memory_mib = 1024`、`max_memory_mib = 4096` 允许实例申请最多 4 GiB，未申请的实例仍使用 1 GiB。CPU 支持 1 或不超过 32 的偶数，内存范围为 128–65536 MiB；超出范围或授权上限会报告不同错误。管理员修改 profile 后需重启管理服务；先停止所有活动 VM 及其监督服务。

运行中的实例不能热调整。修改私有 YAML 后执行 `./vmm.sh environment check FILE.yaml`，再执行 `./vmm.sh environment apply FILE.yaml`；资源授权预检查在停止实例之前完成。`apply` 也会按原语义重新应用 YAML 的 Git 环境配置。更新服务 manifest 时，按原流程停止监督服务、重新安装服务包、启动。CPU/内存变更复用原持久根盘，不需要销毁 VM 或迁移数据。

请求随服务包保存，监督恢复、宿主重启和 `reboot` 会保留。启动器将同一有效资源同时用于 Firecracker、VMM cgroup 与 virtiofsd cgroup。`list` 的 VCPUS/MEM_MIB 列及 JSON 的 `resources` 显示活动实例的有效资源；管理 API 另返回 `requestedResources`，表示实例覆盖值。

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
| `proxy` | 可选，引用宿主管理配置中已授权的代理名称 |
| `command` | guest argv 数组，不经过隐式 shell |
| `working_directory` | guest 绝对工作目录 |
| `environment` | guest 环境变量表，值必须是字符串（数字也加引号），随私有服务包保存 |
| `files` | 包内相对 source、guest 服务目录下相对 target；可选 executable、overwrite |
| `ready_port` | guest loopback TCP 就绪端口；启动前拒绝已有监听者 |
| `startup_timeout_secs` | SSH/应用就绪等待上限，1–300 秒，默认 60 |
| `publish` | 通过现有 API 发布的 TCP/UDP 映射，只在服务就绪后发布 |

`{service_dir}` 可用于命令参数与环境变量，展开为 `/home/agent/.local/share/firecracker-services/SERVICE_ID`。单个包最多 64 个文件、总计 32 MiB；更大的程序可通过授权直通目录读取。文件默认仅在目标不存在时投递；guest 内已有文件（包括升级前已存在、没有部署记录的文件）原样保留，重启不会覆盖用户编辑。缺失文件会补回，不执行目录镜像删除。显式设置 `overwrite = true` 的文件在每次准备/启动时以宿主快照覆盖；默认是 `false`，适用于在 guest 内维护的配置。

`./vmm.sh service update VM [SOURCE/service.toml --identity KEY] [--enable]` 是源码脚本提供的单命令配置更新。首次完整调用成功后，将源 manifest 和 SSH 身份的绝对路径保存在调用用户的 `~/.config/mindos-firecracker/services/VM.json`（0600，仅路径），后续只需 `./vmm.sh update VM`（兼容 `./vmm.sh service update VM`）。显式参数覆盖记忆值；`~/` 始终按调用用户展开；失败不更新记忆。它先建立私有源快照、检查服务 ID、持久根盘和存储布局，再调用既有 stop/install/up 固定命令。仅本次服务包中声明的文件设置 overwrite，不修改原 manifest，不删除其他文件、Session 或根盘。失败保留已完成阶段，不自动重放；修复原因后可显式重新执行。旧版安装的 Rust 管理 CLI 也可复用这条脚本入口，无需重启全局 Firecracker 管理服务。验证：`python3 scripts/test-update-service.py`（测试进程置于 temporary-process 作用域）。

安装将文件与 SSH 私钥快照保存到宿主 `/var/lib/mindos-firecracker/services/packages/`，权限由私有根目录和 0600 文件约束；后续启动不依赖原始服务包或用户的 SSH 私钥路径。更新宿主服务包不会自动覆盖 guest 已有文件；需要宿主管理某个文件时，在它的 `[[files]]` 中显式声明 `overwrite = true` 后重新安装服务包。同 ID 同内容重复安装幂等；更新包要求所有绑定 VM 的监督服务均停止。profile、挂载等存储变更仍受持久根盘兼容检查限制。服务包和业务凭据不应提交到仓库。

Git 开发环境还需要在 guest 的 agent 用户下配置 `git config --global user.name` 和 `git config --global user.email`，并执行 `git lfs install --skip-repo`。镜像提供 `git`、`git-lfs` 和 `openssh-client`；个人提交身份、出站 SSH 私钥及可信 `known_hosts` 通过私有部署配置提供，不写入 Dockerfile 或源码。出站 Git 密钥与登录 VM 的管理密钥用途独立；只投递实际需要的密钥和对应 SSH Host 配置，不复制宿主全部 SSH 配置。`~/.ssh` 使用 0700，私钥与配置使用 0600。HTTPS remote 使用 SSH 认证时，可在 guest 用户配置中设置 `git config --global url.git@github.com:.insteadOf https://github.com/`，无需改动共享仓库 remote。持久根盘保留 agent home，重建根盘时需按升级流程备份、恢复这些配置；用 `ssh -T git@github.com` 和 `git ls-remote origin HEAD` 分别验证身份和仓库读取权限，不能仅凭密钥文件存在判断认证成功。

### 用一个 YAML 管理用户环境

用户环境统一使用 [environment.yaml](../examples/environment.yaml)，通过 `vmm.sh environment check|up|apply FILE.yaml` 管理。配置引用既有服务 manifest、管理 SSH 身份及 Git 私钥文件，不包含私钥正文；相对路径以 YAML 所在目录解析，绝对路径保持原义，不展开 `~` 或环境变量。YAML 及凭据放在仓库外的宿主私有目录。

```yaml
version: 1
vm:
  id: cell-x1
  profile: services
  proxy: clash
rootfs:
  persistent: true
  user: agent
service:
  manifest: /etc/mindos-firecracker/service-sources/cells-x1/service.toml
  identity_file: /home/li/.ssh/mindos-firecracker
git:
  user:
    name: Rain Li
    email: lizlok@gmail.com
  lfs: true
  github:
    private_key_file: /home/li/.ssh/git_e259
    known_hosts_file: /home/li/.ssh/known_hosts
    https_to_ssh: true
```

profile 继续控制基础镜像与硬件、网络、授权挂载，`rootfs.persistent` 必须与 profile 一致，不会暗中改写宿主全局 profile 或替换旧 rootfs。当前只支持既有 `agent` 用户。服务启动命令、端口、挂载仍由应用自己的 manifest 定义；用户环境初始化由通用 guest agent 执行，不需要修改 Cells 启动脚本。

```bash
./vmm.sh environment check /path/to/environment.yaml
./vmm.sh environment up /path/to/environment.yaml
# Explicitly update an existing VM; stops and restarts this managed service.
./vmm.sh environment apply /path/to/environment.yaml
```

`check` 校验 YAML、文件可读性、密钥权限、GitHub 可信主机、profile 与服务授权，不启动或停止 VM，也不执行网络认证。`up` 安装私有快照并启动/启用服务，首次初始化 Git；已存在的初始化标记会保留 guest 用户修改。已运行服务的配置有变化时拒绝更新，使用显式 `apply`。`apply` 在预检查通过后停止本 VM 的监督服务、更新快照、重新启动；生成一次性的 apply 标记，使本次明确更新生效，后续重启不会反复覆盖用户修改。更新失败会报出具体阶段，不会自动删除根盘或降低授权。

多个 VM 可使用相同服务包与相同环境快照；为避免修改其他 VM 的用户配置，`apply` 拒绝更新绑定多个 VM 的共享服务包，应为不同用户/权限创建独立服务包 ID。新 VM 可复用 YAML 并修改 `vm.id`；更换用户时同时选择独立服务包和密钥。

Git 文件统一保存在 guest 的 `~/.config/firecracker/git/`：`private-key`、`known_hosts`、`ssh_config`、`initialized.json`。目录为 0700，文件为 0600。`~/.gitconfig` 保存提交身份、SSH 配置路径、GitHub HTTPS 映射和可选 LFS 配置，不覆盖其他 Git 配置。私钥源文件名任意，只要求权限不允许组或其他用户读取；只选取可信主机文件中 GitHub 的条目。首次启动要求镜像已有 `/usr/bin/ssh`、Git 和需要时的 Git LFS，缺失会明确拒绝初始化，不自动联网安装软件。`lfs: false` 表示不安装 LFS 用户配置。

验证配置与远程权限：

```bash
./vmm.sh ssh cell-x1 'git config --global user.name; git config --global user.email'
./vmm.sh ssh cell-x1 'ssh -F ~/.config/firecracker/git/ssh_config -T git@github.com'
./vmm.sh ssh cell-x1 'git -C /home/li/share/prj/x1 ls-remote origin HEAD'
```

服务就绪只证明本地初始化与应用启动成功，不代表 GitHub 网络认证成功。管理日志只记录阶段、结果、耗时和关联 operation ID，不输出私钥、完整 YAML、应用环境变量或用户正文。

2026-11-08 实机验收：重新构建包含 Git 2.53.0、Git LFS 3.8.1、OpenSSH 10.6p1 和 Node 26.13.0 的 harness rootfs；镜像 ID 为 `sha256:1de9b2525fa7b91ccb635ea904c31998c9ca945b42cd52c0a3876c819948e80e`。`cell-x1` 在旧持久根盘备份与 agent home 归档校验后创建了新根盘，恢复用户配置和会话文件，再通过私有 YAML 显式应用 Git 环境。新根盘的 lower 与新部署镜像为同一 inode，排除复用旧根盘；标准 SSH 客户端由系统包提供，旧临时客户端与旧出站私钥已清理。相对 worktree 创建/移除、Git LFS、GitHub SSH 认证和仓库 HEAD 读取通过；重启后 Git 配置及初始化标记摘要一致，Cells 项目列表可读。重复 `up` 未重启已就绪 VM，rootfs/profile 不一致在停机前拒绝且失败日志准确。62 项 Rust 测试（串行运行，避免既有端口占用测试的并发竞争）、28 项构建脚本测试及文档检查通过；这些结果不代表完整浏览器页面性能验收。

受管服务请使用 `./vmm.sh service stop VM_ID` 和 `./vmm.sh service start VM_ID`；重启使用 `./vmm.sh reboot VM_ID`。普通 `stop` 仅停止 VM，仍运行的监督服务会尝试恢复它；随后普通 `start` 使用默认 `debian13` profile 和命令行目录创建同名实例，可能与服务包的 profile、挂载或代理配置冲突并返回 `API_ID_CONFLICT`。遇到此错误先查看 `./vmm.sh service status VM_ID`，不要删除持久根盘来解除冲突。

宿主的 `firecracker-workload@VM_ID.service` 监督 SSH，guest 辅助进程监督应用进程组。SSH 控制通道 EOF 会终止应用；失败会清理当前 VM 并由 systemd 重试。就绪事件通过 `sd_notify` 上报，端口全部发布成功后才算启动完成。服务退出、SSH 失败、就绪超时和端口冲突均记录分类日志，原始应用输出不写入管理日志。

管理服务使用 systemd `Type=notify`，完成初始化并绑定 API 监听端口后才发送就绪通知；`systemctl start firecracker-server` 返回后可立即创建或查询 VM。

管理服务正常重启会先停止关联监督 unit，再重建 VM、准备文件、启动服务并恢复端口。`--enable` 控制宿主开机恢复；不加时仍会在本次运行期间监督服务。服务清理按 VM 实例标识核对，拒绝删除同 ID 的替代实例。

例如，保留 guest 自己编辑的配置：

```toml
[[files]]
source = "config.toml"
target = "config.toml"
overwrite = false
```

以上是默认行为。`overwrite = true` 表示持续由宿主管理该文件，后续重启也会覆盖。guest 的 `~/.config/firecracker/proxy.env` 是平台代理配置，仍由所选代理定义更新，不受服务文件的保留策略影响。

## 重启虚拟机

```bash
./vmm.sh reboot VM_ID
# Equivalent native command:
sudo /usr/local/bin/firecracker-server reboot VM_ID
```

受管服务通过 `systemctl restart firecracker-workload@VM_ID` 完整停止并启动，保留开机启用状态，等待服务 ready 后返回。普通 VM 保留原 profile、目录、卷和代理选择，停止后重新创建，并恢复原端口映射；此时仅确认 VMM 已启动，应用需要自己的开机启动机制。持久根盘和额外卷保留，临时根盘的修改会按 profile 策略丢弃。未运行的 VM、缺少参数或多余参数会明确报错。

普通 VM 重启会分阶段记录 stop/create/restore_ports 的结果；端口恢复失败时保留已重建的 VM 并报告具体阶段。重启期间不应由其他管理客户端同时停止或替换同一 VM。

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

回归：`cargo test --locked`、`cargo clippy --all-targets -- -D warnings`、`python3 scripts/test-build-environment.py`，其中涉及测试进程的检查应放在仓库 temporary-process 有界作用域内执行。


## 按 VM 选择代理

管理员在 `/etc/mindos-firecracker/config.toml` 顶层配置 helper 和代理定义：

```toml
proxy_egress = "/usr/local/libexec/mindos-firecracker/proxy-egress"

[proxies.clash]
http = "http://{host_ip}:7890"
socks = "socks5h://{host_ip}:7890"
no_proxy = ["localhost", "127.0.0.1", "127.0.0.0/8", "::1", "192.168.31.0/24"]
```

在已有 `[profiles.services]` 表内加入 `allowed_proxies = ["clash"]`；服务包根表加入 `proxy = "clash"`，停止该服务、重新 `service install` 后再启动。也可通过创建 VM 的 API 提供 `"proxy":"clash"`，但应用环境和 shell 配置由服务管理层投递。

`{host_ip}` 按 VM 槽位替换：第一台通常为 `10.204.0.1`，后续为 `10.204.1.1` 等。Clash 必须监听该地址或 `0.0.0.0:7890`。也支持明确的外部 IPv4 代理地址；当前不接受域名和 URL 凭据。

服务进程和新登录 shell 会获得大小写两套 `http_proxy`、`https_proxy`、`all_proxy`、`no_proxy`。shell 文件位于 `~/.config/firecracker/proxy.env`，`.profile`/`.bashrc` 追加一次引用，已有内容保留。选择代理时禁止在服务 `environment` 中重复声明这些键；取消引用后会清理托管的 shell 配置。

网络层只为选择了该代理的 VM 放行指定地址的 TCP 端口，停止时撤销并清除相关 conntrack。`no_proxy` 仅控制应用绕过代理，不额外放行私网；默认隔离仍生效。这是应用代理环境配置，不是所有协议的透明代理，不读取代理环境的应用需自行配置。
