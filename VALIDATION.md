# Firecracker server 验收记录

统一管理入口为 `vmm.sh`，无参数显示帮助，显式 `build` 使用 release 构建。兼容脚本已删除，下文命令统一使用当前入口名称。

OS 构建现需显式 `--os`：默认 `build` 只构建 release 程序制品并刷新校验清单，不构建或压缩 kernel/image。`setup`、`network`、`shared-root` 默认复用已有 OS 文件，缺失时在执行初始化副作用前提示传入 `--os`。新增 6 项构建选择回归，部署/构建测试共 24 项通过，验证默认构建保持已有 OS 文件内容和修改时间、无 OS 制品时可构建程序、显式 OS 构建阶段选择、初始化参数传递及未知参数拒绝。

## 2026-11-08：实例 CPU/内存覆盖与 cell-x1 扩容

- YAML `vm.resources`、服务 manifest `[resources]` 与创建 VM API 支持可选 `vcpus` / `memory_mib`，缺省字段继承 profile；旧授权默认不变，管理员通过 `max_vcpus` / `max_memory_mib` 显式提高上限。请求随服务包保存，启动器使用同一有效 profile 配置 Firecracker 和两类 cgroup；列表展示有效资源。
- 69 项 Rust 测试、Clippy（all-targets，warnings 为错误）、fmt、文档检查通过；核心性能回归 9 个文件 57 项及预算规则 6 项通过，临时作用域已清理。这些结果不代表完整浏览器页面性能验收。
- 宿主私有 `/etc/nestos/firecracker/environments/cell-x1.yaml` 已声明 `memory_mib: 4096`；`services` 默认保持 1024 MiB，上限显式设为 4096 MiB。停止监督服务与管理服务后仅更新运行程序，使用 `environment up` 保留 Git 初始化状态；镜像与根盘未重建。
- 运行中的管理程序与新 release 二进制 SHA-256 一致。`cell-x1` 的管理状态为 2 vCPU / 4096 MiB、ready，guest `free -m` 总计 3938 MiB；VMM memory.max 为 6710886400 字节（含原共享根缓存额度），virtiofsd 为 4429185024 字节，两者 cpu.max 均为 `200000 100000`。根盘 inode 3839567、2 GiB 大小及 Git 配置摘要在扩容与再次 reboot 后一致，18787→8787/tcp、代理、enabled 状态保留。
- 真实失败验证：未提高上限前 4096 MiB 的 check 被拒绝；提高上限后 8192 MiB 的 apply 在停机前以 `RESOURCE_MEMORY_NOT_AUTHORIZED` 拒绝，监督 runtime 记录不变。错误日志包含操作 ID、阶段、结果、分类和耗时，无凭据正文。临时 YAML 已移除，部署备份保留在宿主私有 `resource-upgrade-cell-x1-20261108` 目录。

## 2026-11-07：通用服务托管、持久根盘与 cell-x1

后续补充顶层 `firecracker-server list [--all] [--json]` 和 `vmm.sh list`。已更新 release 二进制及已安装辅助脚本，在普通用户下直接执行成功，显示 `cell-x1` 为 `vmm-running / ready`、IP `10.203.0.2`、持久根盘及 `127.0.0.1:18787->8787/tcp`。列表只调用一次管理 API；本机单次快照耗时为 17–21 ms，不是前端页面性能验收。Python 服务回归现为 23 项，新增默认/全部列表、128 VM 单次 API、实例身份校验、无敏感信息输出和查询失败不得当作空列表的覆盖。Rust 26 项、部署 18 项、clippy/fmt 与语法检查通过；SSH 连接同时复测通过。

在现有真实 KVM/网络环境上，仅更新管理程序、宿主辅助脚本及 systemd unit，没有重建或替换 Debian 镜像、内核或 fc-hp。Debian 镜像更新前后 inode 均为 `2924520`，SHA-256 均为 `c3f5fb4fdcb7387472f2c0c6c343698d29595bfd9ad638388a0947bd40b59c57`。

新增通用服务包管理与显式持久根盘配置，服务命令、文件和环境均来自管理员安装的外部定义。实现及通用 Python HTTP 示例位于本仓库；cells 的实际服务定义和凭据仅保存在宿主私有 `/etc/nestos/firecracker/service-sources/cells-x1/`，核心没有 cells 分支。

- Rust `cargo test --locked` 共 26 项通过，覆盖有限/无限生命周期、持久 private/shared 根盘复用、lower/内核/initrd 固定、存储配置冲突、活动根盘禁止删除、路径校验及既有 API 边界。
- 通用服务 Python 回归 17 项、部署回归 18 项、端口发布回归 10 项通过。覆盖 guest 文件投递、包快照、只读/相对路径约束、服务更新锁、同 ID 实例替换保护、命令退出、已有监听拒绝、SSH EOF 回收、上传超时及不完整事件不能阻塞超时。
- Rust fmt/clippy（全部 target，warnings 为错误）、Bash/Python 语法、systemd unit 校验与 diff 空白检查通过。
- 创建并保留 `cell-x1`，使用通用 `services` profile：2 vCPU / 1024 MiB、2 GiB 持久 upper、同路径直通 `/home/li/share/prj`，显式无限生命周期。已有网络槽位复用，未更新 guest 镜像。
- cells 0.4.4 通过复制的专用配置和启动脚本运行；身份为 `cell-x1`，通过宿主 `127.0.0.1:18787` 访问。认证、文件读取、进程执行、projects/sync/harness 能力发现通过。未认证请求返回 401。
- 宿主原 cells 继续使用 8787。并行验证将 x1 等已有业务导出设为只读，独立 `vm-write` 导出 `/home/li/share/prj/tmp/cell-x1` 可写。真实 bubblewrap 命令在 guest 中写文件，宿主和 cells HTTP 均能读取。
- 宿主端口占用返回冲突；尝试删除活动 VM 根盘返回 409。首次只读导出与遗留可写 grant 冲突时，cells 明确拒绝启动；修正外部配置后恢复，失败未删除持久根盘。
- 分别重建 VM、重启管理服务，以及停止管理服务后通过 `install-server` 更新再单独启动，均自动恢复 cells 和端口。普通 `/home/agent/.config` 文件、工具 home 文件及 `/etc/ssh` 主机身份保持一致；cells 进程 epoch 发生变化，确认是重新启动。
- 对受管 workload 的宿主监督主进程发送 SIGKILL 后，systemd 自动重启监督；SSH 断连使 guest 应用退出，新监督复用原 VM 成功恢复服务与映射。该故障注入未强杀 VMM/管理服务，也不代表完成了宿主断电一致性验证。

受管服务保持 active/enabled，宿主开机和管理服务启动依赖均已启用；没有实际重启宿主。启动阶段的 TCP 就绪不等于前端页面性能验收，本次没有修改 UI/RPC 路径，也未声明页面性能达标。没有执行真实模型请求或迁移宿主 harness 认证凭据。

HTTP/SSH 验收驱动和会创建测试子进程的回归均使用仓库 temporary-process 有界作用域，已等待关闭回执。验收用的用户目录标记、共享目录标记文件和包含认证信息的临时上下文均已删除；保留的 `cell-x1` 是用户要求的受管部署，使用正式 systemd/VM 生命周期。无凭据的分阶段结果记录位于 `/var/tmp/fc-cell-x1-{initial,recreated,manager-restart,final-deploy,supervisor-recovery}.json`；详细服务日志使用 `journalctl -u firecracker-workload@cell-x1` 查看。

日期：2026-10-24。环境：Linux x86_64、真实 `/dev/kvm`、cgroup v2、Debian 13 guest、Linux 6.18.54 guest 内核、Rust virtiofsd 1.14.0。测试仅使用临时授权目录和根盘副本；没有连接真实 Codex 凭据或 session。

## 构建与契约

- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过。
- `cargo test --locked`：14 项通过，覆盖认证、Origin、参数/路径注入、目录授权与互斥、只读块设备、vsock 握手与 stdin EOF、真实 ext4 导入和目录替换后的 FD 固定。
- `cargo build --release --locked` 通过。
- Shell/Python 脚本语法检查通过。
- `prepare-fc-hp.sh` 从上游拉取固定基线并应用补丁，20 个修改文件与已构建的 fork 逐字节一致。

## 真实 KVM

以下两组脚本使用表内 server release 二进制均通过，最终测试进程、临时 jail 和 VM/backend cgroup 已回收。

`scripts/smoke.py` 覆盖：Debian 13/agent UID 1000、可写卷、只读卷、正常关机后重启读取未手工 sync 的文件、ext4 流式导出、supervisor 被 SIGKILL 后回收 VMM、管理进程被 SIGKILL 后回收 VMM。正常关闭必须返回 `mode=guest-shutdown`，强停不会被视为落盘成功。

`scripts/smoke-virtiofs.py` 使用 fork 的 musl release 和默认 seccomp，覆盖：

- 宿主目录直接读写、rename、session 文件落宿主，宿主修改能被 guest 读取。
- 服务固定源 FD 后将原目录改名、替换原路径为符号链接，仍只导出原目录。
- 非递归 clone 排除宿主子挂载；指向导出范围外的符号链接不能读取宿主文件。
- 并发导出同目录或直接 bind 子目录形成的别名返回 409。
- 在一次性测试镜像里由 guest root 把只读 virtio-fs remount 为 rw，create/unlink/rename 仍被宿主只读授权拒绝。
- vhost-user backend 接受连接但不回复时，创建在共享启动期限内失败，VMM/daemon 全部回收。
- backend 崩溃、分别 SIGKILL VMM supervisor 和 backend supervisor、SIGKILL 管理服务后，检查进程组/cgroup 为空。
- 管理日志不包含测试 API key。

原始设备层测试另在 fork 中通过：启动后配置拒绝、创建快照前拒绝且不留下快照文件、daemon 断开令 VMM 非零退出；管理服务不提供快照或热插拔接口。

## 本机证据与制品

服务验收摘要保存于 `/var/tmp/firecracker-server-block-acceptance.log`、`/var/tmp/firecracker-server-virtiofs-acceptance.log`。设备层证据在 `/home/li/share/prj/virtiofs-validation/final-keyboard/`。系统日志和测试源文件不随仓库提交。

| 制品 | SHA-256 |
| --- | --- |
| server release | `4d52590e2e99dd1ffde356f8c24f47d1ea44d1dab33b4b02ec52f187d14741b9` |
| Firecracker fork musl release | `2e67c23d06a8320af90aa610a06b30fc57a2fb4191258c00494b9db94db68e27` |
| 对应 jailer | `058be3eae177c0aa11b2c038b31f774ad0e5a19a4272adfdf6a8231890560b96` |
| Linux 6.18.54 vmlinux | `dbbdab9311284c3107c173abe685f576106e844d85d437879727dd8af45e7f34` |
| Debian 13 完整服务镜像 | `85e0b237ba7f1ceb77af5f716721b442dfb3dcc0f47bba032f18133573eab872` |
| Rust virtiofsd | `2272d8bc4502b2abe2d6509b2bc0db6af8cd1cf02de9628fc883ef4ca6fcfd60` |
| 仓库 VMM 补丁 | `4112acb7ae58bfd5283ec9006c235e274cdaa34e8b1393080719e199ed71664b` |

Debian 制品位于 `/var/tmp/nestos-firecracker-debian13-clean.ext4`。基础镜像中的 Docker 标记必须在 export 后删除；否则 systemd 误判容器而跳过完整的 VM 卸载流程。`acpi=off` 下使用正常 reboot/i8042 reset 退出 VMM，不能依赖 poweroff/halt。

## 当前范围

每台 VM 一个 virtio-fs 导出，可混用多个持久块卷；不支持 DAX、热插拔、快照恢复或 daemon 重连。直通无需文件复制同步，但不提供应用事务、写入冲突合并、文件事件通知保证或凭据隔离。没有进行性能基准、aarch64 guest 验收、实际 LLM 联网调用或现有 cells harness driver 的 launcher 集成。本记录验证的是独立隔离服务及其管理/字节流接口。

## 并发审查修复

本次增加资源预留、同资源串行、慢导入期间查询/停止响应、失败恢复及关闭等待的四项确定性测试。`cargo test --locked` 共 18 项通过，`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 与 release 构建通过。

修复后的 release 已重新通过两套真实 KVM 脚本：块设备场景覆盖读写、只读、持久卷、supervisor/管理进程崩溃回收；virtio-fs 场景覆盖实时读写、源 FD 固定、排除子挂载、宿主只读、启动期限和 backend/supervisor/管理进程崩溃回收。两套日志均确认没有测试密钥泄露，临时 VM/jail/cgroup 已由脚本清理。

当前 server release SHA-256 为 `8a44025a6e1837d22492602c74e03c1540cefe5d6e1a114efecb0dbf5b015c24`；内核、guest 镜像与 virtio-fs VMM/daemon 沿用上表制品。新证据位于 `/var/tmp/firecracker-server-concurrency-block-acceptance.log` 和 `/var/tmp/firecracker-server-concurrency-virtiofs-acceptance.log`，上表旧 server 哈希仅对应最初验收。

## 自动环境构建

`vmm.sh` 使用相邻 `../fc-hp`，完成 musl Firecracker/jailer、Cargo virtiofsd 1.14.0、Linux 6.18.54 和 4096 MiB Debian 13 ext4 构建。内核从清华镜像下载源码并通过固定的 kernel.org SHA-256 校验；guest APT 使用中科大镜像。再次构建复用 ext4，制品摘要写入忽略的 `build/SHA256SUMS`。

- Bash 语法检查、ShellCheck、Python 编译检查通过。
- `python3 scripts/test-build-environment.py`：7 项通过，覆盖重复安装保留配置/密钥、文件权限、失败复制不破坏原制品、符号链接拒绝、既有目录所有权保留及 UID 段冲突。
- 私有 mount namespace 中实际安装、再次安装、`check-environment.py` 全部通过，确认配置和密钥保留；没有在宿主安装或启动长期服务。
- `./vmm.sh smoke` 使用新内核、根盘及 Cargo daemon 通过完整真实 KVM/virtio-fs 验收：实时读写、源 FD 固定、排除子挂载、宿主只读、启动期限，以及 backend/supervisor/管理进程异常回收；测试 VM、临时目录和 cgroup 已清理，日志未泄露测试密钥。
- guest 内核显式关闭 `CONFIG_FUSE_DAX`。此 fork 没有 DAX shared-memory region，开启时 virtio-fs 驱动探测报 `could not reserve region addr=0x0 len=0x0`，随后挂载报 tag not found。

新构建制品 SHA-256（server、Firecracker/jailer 沿用上述版本）：

| 制品 | SHA-256 |
| --- | --- |
| Linux 6.18.54 vmlinux | `9a2a66b4fb629cf965ef530ab2c85ec5acd18071a28838af3d759363e1f570ff` |
| Debian 13 4096 MiB ext4 | `28d1c8d6222c1c23a533e9e78a38c41dff41144295fc44e0a9ce621ff202c50a` |
| Cargo virtiofsd 1.14.0 | `e42ad646ef58e05f8aecb5b340a590d88964653b35d41cca4da1336f863122c3` |

构建及安装验证日志：`/var/tmp/fc-server-build-environment.log`、`/var/tmp/fc-server-install-environment-test.log`。真实 KVM 日志：`/var/tmp/fc-server-build-environment-smoke.log`。内核配置差异诊断：`/var/tmp/fc-server-build-environment-boot-dax-diagnostic.log`。

## 2 GiB 精简镜像

默认根盘改为 2048 MiB，提供 `base` 与 `harness` 两个构建目标。基础版保留 systemd、udev、Bash、Git、Python、vsock 与网络基础组件；裁剪文档/翻译并保留版权文件，禁用后台维护定时任务、getty 和等待联网，journald 使用最多 16 MiB 的内存日志。运行版另从锁定依赖的构建阶段复制 Node 与 Codex/Claude Code/DSH，最终镜像不含 npm、开发包或构建工具。

`python3 scripts/test-build-environment.py` 共 11 项通过，新增验证 guest 配方、容量和 flavor 改变会使缓存失效、本地 node_modules 不参与配方摘要，以及部署保留稀疏文件和原内容。构建输出原始镜像、输入记录、容量/占用/版本报告、zstd 发布包和 SHA-256。

基础版首轮实测（149 个 Debian 包，不含 Node/三个工具）：文件数据分配约 268.1 MiB，含 ext4 元数据/日志已用 370.2 MiB，宿主稀疏文件占用 271.4 MiB。该版通过完整真实 KVM/virtio-fs 验收，记录创建到 VMM 启动 283 ms，创建到 guest 完成文件读写命令 2849 ms。最终版增加 bubblewrap 后为 150 个 Debian 包，已用 370.4 MiB、宿主占用 271.5 MiB、zstd 包 78.7 MiB。该记录为本机单次样本，包含镜像复制及轮询，不代表性能分位数或真实模型请求耗时。

基础版日志：`/var/tmp/fc-server-lean-base-build.log`、`/var/tmp/fc-server-lean-base-smoke.log`，制品位于忽略的 `build/variants/base/`。

默认运行版使用 Node 24.22.1，预装 Codex 0.164.0、Claude Code 2.1.209、DSH 0.2.0-rc.2。固定依赖通过 BuildKit/npm 镜像源下载并校验 lockfile 的完整性；npm 缓存不进入最终镜像。构建后以 UID 1000、无网络/凭据探测四个版本，确认无 npm、无 Debian 开发包且后台维护定时任务已屏蔽。

| 运行版制品/指标 | 实测 |
| --- | --- |
| 根盘容量 | 2048 MiB |
| ext4 已用（含元数据/日志） | 1668.8 MiB |
| 宿主稀疏文件占用 | 1577.1 MiB |
| zstd 发布包 | 441.3 MiB（462764306 bytes） |
| 创建到 VMM 启动 | 1039 ms |
| 创建到 guest 文件读写命令完成 | 3595 ms |
| guest 内 Node/Codex/Claude/DSH 版本探测 | 30 / 148 / 397 / 95 ms |

运行版通过真实 KVM 块卷和完整 virtio-fs 验收（含持久卷重启后读回、正常关机、只读限制和崩溃回收），四个版本在真实 guest 内均通过。安装与重复安装在私有 mount namespace 再次通过，确认四个绑定目录存在，配置和密钥保留，没有在宿主安装或启动长期服务。再次执行同参数构建，确认命中输入缓存且 ext4 的 inode、mtime 和容量均未改变。Bash、ShellCheck、Python 编译检查通过。没有执行需要模型凭据的真实 LLM 请求。

当前根盘 SHA-256 为 `39f51c3425afdf7c4459e6ce95596636610a203383d839d8266d9a138a9a8455`，其 `.zst` 为 `442e9243b712f9d0f46e9cda6cb0be3a102dffa4e3e146749f80c4fb7d60d98c`。详细输入、容量和版本记录见 `build/images/debian13.ext4.json`，全部制品校验见 `build/SHA256SUMS`。

运行版日志：`/var/tmp/fc-server-lean-harness-build.log`、`/var/tmp/fc-server-lean-harness-smoke.log`、`/var/tmp/fc-server-lean-harness-block-smoke.log`、`/var/tmp/fc-server-lean-install-test.log`、`/var/tmp/fc-server-lean-cache-reuse.log`。

## 已部署网络和 SSH

在用户启动的管理服务上配置 4 个独立网络槽位，guest 使用 `10.203.N.0/30`，veth 使用 `10.204.N.0/30`，通过 `enp1s0` 做 IPv4 NAT。每个 netns 独立运行 systemd 管理的 DHCP/DNS，使用宿主可达的公共 DNS。管理服务、网络与 DNS 单元均已配置开机依赖；未清空或修改 Docker 的现有防火墙表和原有 namespace。

镜像加入 OpenSSH，仅允许 agent 公钥登录，专用私钥位于宿主用户的 `~/.ssh/nestos-firecracker`（0600），公钥由构建脚本注入，输入摘要记录公钥哈希。每个新根盘首次启动独立生成 SSH 主机密钥，客户端使用本次 VM 实例标识区分复用的 IP。根盘仍为 2147483648 bytes，新增 SSH 后 zstd 包为 466018848 bytes，记录见当前 `build/SHA256SUMS` 和 `debian13.ext4.json`。

- `python3 scripts/test-build-environment.py`：15 项通过；新增私钥不可进入镜像、64 个槽位的地址/MAC 独立、loopback DNS 拒绝、宿主路由冲突拒绝。
- nftables 两层规则经过 `nft --check` 校验并实际应用。SSH/PTY、DNS、公网 HTTPS 在两个临时 VM 上通过；两台 VM 的主机密钥不同。
- guest 到宿主 `192.168.31.10`、路由器 `192.168.31.1`、metadata `169.254.169.254` 和另一台 VM 的连接均被阻断。
- 两台测试 VM 在 POST 返回后约 2381 / 2378 ms 可通过 SSH 执行命令，测试后均正常关机回收。测试记录不包含真实模型认证请求。
- `./vmm.sh start/ssh/stop` 已由普通宿主用户实测，guest 为 UID 1000，CLI 测试 VM 正常关机。为用户保留的 `demo` 挂载 project，地址 `10.203.0.2`，三个运行时版本验证通过。
- 管理服务最初启动后的双 VM 根盘测试也通过：A 写入文件后 B 不存在该文件，B 写同名文件不改变 A；两台均正常关闭。

日志：`/var/tmp/fc-server-ssh-image-build.log`、`/var/tmp/fc-server-network-deploy.log`、`/var/tmp/fc-server-network-check.log`、`/var/tmp/fc-server-network-smoke.log`。当前实际部署已包含网络/SSH，与前文仅在 mount namespace 内测试安装的阶段不同。

## 2026-10-25：共享根盘物理内存

已实施并部署 `virtio-pmem + ext4 DAX + OverlayFS`。复用 fork 已有 PMEM 设备，不修改其 virtio-fs 协议；基础镜像为 root:root / 0444，同一 inode 硬链接进各个 jail，并使用只读 KVM 内存槽。每 VM 的私有 ext4 upper 按需占用磁盘，默认容量 2 GiB，随 VM 清理。私有副本模式继续支持从只读基础镜像创建可写根盘。

- `FC_SHARED_TEST_VMS=100` 真实 KVM 验收通过：100 个同时运行的 VM，每个 1 vCPU / 256 MiB，无网；第一个 VM 同时使用 live virtio-fs 项目目录。全部 VM 完成 Node 文件 DAX mmap、读取所有文件页，以及一个私有 mmap 页的修改。
- Node 文件大小 126879696 bytes；每个 guest pagemap 检查到 30569 个 PMEM 页、408 个稀疏文件零页和 1 个私有 COW 页。`statx` 确认基础文件启用 DAX，零页通过 guest `kpageflags` 的 `KPF_ZERO_PAGE` 识别，避免把零页误算为未共享的文件页。
- 宿主 pagemap 对比全部 100 个 VMM：采样时共同驻留的基础镜像页共 **87027712 bytes（82.996 MiB）**，对应偏移处 PFN 差异为 0；各 VM 的基础映射驻留量为 83.023..160.719 MiB。共同驻留量随回收变化，这不是整张 2 GiB 镜像的内存占用，也不是最大共享量。
- 两 VM 的最终回归采样共同驻留 **180617216 bytes（172.25 MiB）**，对应 PFN 差异为 0。修改同一 `/home/agent/.bashrc` 后，各自读取到自己的内容；guest root 直接尝试写 `/dev/pmem0` 后，宿主基础镜像 SHA-256 保持不变。停止并重建 VM 后，私有修改消失。
- 100 个 VM 全部正常关机回收。该测试验证基础系统、共享 mmap 和隔离；未运行 100 个并发 Codex/Claude/DSH 模型会话。guest 内核、堆、栈、普通 read 缓冲和 OverlayFS copy-up 仍独立占用资源；文件 copy-up 是整文件复制。
- `cargo test --locked`：4 个库测试及 15 个边界测试通过。构建/部署 Python 回归 17 项通过，包含迁移幂等、保留网络配置、自定义镜像拒绝、100 槽位独立性。ShellCheck、Bash/Python 语法及真实 KVM 块卷、virtio-fs 完整回归通过。
- 本机已通过 `./vmm.sh shared-root` 切换内置 profile，原配置备份到 `/etc/nestos/firecracker/config.before-shared-root.toml`；API key、目录绑定和 4 个网络槽位核对一致。服务保持启动并开机启用；日常并发上限仍为 4，程序支持配置到 128。
- 部署后的共享模式通过 SSH/PTY、不同主机密钥、DNS/HTTPS、宿主/私网/metadata/跨 VM 阻断验收；两个 VM 创建后约 2402 / 2405 ms 可执行 SSH 命令，随后正常回收。
- 最后从已部署共享根盘经 SSH 执行镜像检查：Node v24.22.1、Codex 0.164.0、Claude Code 2.1.209、DSH 0.2.0-rc.2 均通过，版本探测约 7 / 65 / 186 / 80 ms。测试 VM 正常关闭，没有留下活动 VM。
- 两 VM 组合验收同时挂载 PMEM 共享根盘、live virtio-fs 和独立 virtio-blk 持久卷；停止并重建后，根盘私有修改消失，持久卷中的 `durable` 标记保留。此项验证使用独立测试目录，没有读写生产绑定数据。

新 initramfs 为 1063646 bytes，SHA-256 `9e931aa967ec13b6a4543c2c019147149055546e0d0d14b4a140f6332acd2cd5`；内核 SHA-256 仍为 `9a2a66b4fb629cf965ef530ab2c85ec5acd18071a28838af3d759363e1f570ff`。基础镜像容量 2147483648 bytes，文件系统已用 1763086336 bytes，zstd 包 467108564 bytes；完整版本、输入和哈希见 `build/images/debian13.ext4.json`、`build/SHA256SUMS`。

日志：`/var/tmp/fc-shared-100-smoke.log`、`/var/tmp/fc-shared-regression-smoke.log`、`/var/tmp/fc-shared-persistent-smoke.log`、`/var/tmp/fc-shared-final-build.log`、`/var/tmp/fc-shared-deploy.log`、`/var/tmp/fc-shared-network-smoke.log`、`/var/tmp/fc-shared-environment-check.log`、`/var/tmp/fc-shared-runtime-check.log`。

## 2026-10-25：nftables TCP/UDP 端口发布

已实现并部署 `./vmm.sh publish VM HOST:GUEST[/tcp|udp] [--listen IPv4]`、`unpublish VM HOST[/tcp|udp]` 和 `ports VM`。默认只绑定 127.0.0.1，显式指定 0.0.0.0 时允许宿主所有 IPv4 接口入站；两端端口为 1..65535，每 VM 最多 16 条。发布使用独立管理 API，profile 的 `ports` 继续表示 vsock 授权。

- 真实 VM 与原生 CLI 验收通过：宿主 loopback 的 TCP/UDP，以及隔离外部客户端 netns 经宿主 `198.18.255.1` 的 TCP/UDP，均到达正确 guest 的 8080 服务；绑定 loopback 的映射不能从这个外部网段访问。
- 本机应用已占用端口、另一个 VM 的 wildcard 端口均被拒绝为 `HOST_PORT_IN_USE`；相同映射重复发布不增加记录。管理服务保留绑定 socket，但转发数据直接走内核 DNAT/SNAT，不经过代理进程。
- 未发布的 guest 8081 服务仍被阻断；guest 访问宿主发布端口仍被私网隔离阻断。撤销 TCP/UDP 映射后不可访问，API/CLI 查询为空。
- 同 VM ID、同 IP 槽位重建验证通过：旧规则没有恢复；复用同一个 UDP 客户端 socket，在显式重新发布前无法访问新 guest，重新发布后才得到新实例的标记。新增映射会定向清理访问该监听端口产生的无 NAT 连接，修复旧 UDP conntrack 导致的新规则不生效问题。
- 映射删除先撤销 active conntrack mark，再清理对应 root NAT flow；VM 停止还清空所属 netns 的连接跟踪，然后才释放 UID/IP 槽位。mark 前缀为 `0xfc000000/0xff000000`，只在本服务的受管接口处理。更新仅重建有变化的 netns 规则，服务内串行执行 nft 操作，不持有全局资源目录锁等待子进程。
- 管理服务 SIGKILL 故障注入通过：supervisor 回收 VMM，systemd ExecStopPost 清除发布记录和规则；测试只清理自身已退出 VM 的遗留 jail/cgroup 后重启服务。新建同 ID/IP 的 VM 没有旧映射，明确重新发布后 TCP/UDP 正常。故障注入需使用脚本的 `--crash`，并要求管理服务没有业务 VM。
- Python 10 项发布回归通过，包含规则作用域、wildcard 冲突、实例隔离、无 NAT UDP 清理、变更 netns 选择、发布重试幂等、应用失败回滚和回滚失败时撤销 active mark/保留恢复日志。已有构建部署回归 17 项通过。Rust 7 项库测试及 15 项边界测试通过，包含绑定保留/释放、TCP/UDP 独立冲突与请求字段约束。
- 原有 SSH/PTY、独立主机密钥、公网 DNS/HTTPS 和宿主/私网/metadata/跨 VM 阻断回归通过。对比变更前后的 nftables JSON，除本服务拥有的两个表及实时计数外，宿主/Docker 规则保持一致。ShellCheck、Bash/Python 语法、Rust fmt 和 diff 空白检查通过。

当前映射配置只对运行中的 VM 实例生效；服务/VM 重建后不自动恢复。IPv6 和端口范围未实现。为适配 netns 的输入白名单，guest 看到 SNAT 后的槽位宿主地址 `10.204.N.1`，不保留客户端原始 IP。第三方纯 DNAT 规则没有监听 socket 时，需要管理员协调端口分配。

服务保持 active/enabled，测试 VM、模拟外部客户端 namespace 和端口映射已清理。日志：`/var/tmp/fc-publish-final-build.log`、`/var/tmp/fc-publish-final-install.log`、`/var/tmp/fc-publish-rust.log`、`/var/tmp/fc-publish-python.log`、`/var/tmp/fc-publish-final-smoke.log`、`/var/tmp/fc-publish-network-regression.log`、`/var/tmp/fc-publish-check.log`。

## 2026-11-07：Rust 运行期迁移与按 VM 代理

宿主网络、端口发布、服务包安装/监督、列表和代理规则迁至 Rust；guest 使用静态链接的 `firecracker-guest`，通过 SSH 投递，不改基础镜像。删除旧运行期 Python 实现，构建/离线安装/诊断脚本保留 Python。升级备份位于 `/var/backups/nestos-firecracker/before-native-runtime/`，仅 root 可读。

- Rust 47 项测试通过；覆盖包快照、路径/凭据日志约束、代理授权和槽位规则、端口冲突、guest 进程组回收、控制 EOF、就绪超时及持久根盘。构建/安装回归 26 项通过，包含旧 helper 迁移幂等、配置备份、已知 pyc 清理和缺少静态 guest 制品时在修改系统前失败。`cargo clippy --all-targets -- -D warnings` 通过。
- 实际安装后的辅助程序为 ELF，网络和工作负载 systemd unit 使用 Rust CLI，guest 监督进程为静态 ELF；已安装 helper 目录没有 Python 源文件或字节码。没有卸载 Python：业务服务和离线构建仍可自行使用它。
- `cell-x1` 使用 `services` profile、`cells-x1` 私有服务包、`clash` 代理。大小写两套代理环境传给服务和新登录 shell；guest 经 `http://10.204.0.1:7890` CONNECT/TLS 访问 `https://example.com` 返回 200。宿主 TCP 22、私网和其他 VM 地址仍被阻断，未授权 profile 请求代理返回 403。
- cells 认证 capabilities/exports API、bubblewrap 子进程执行、直通目录写入通过。cells 普通 process API 的环境遵循业务工具自身策略；不把它视为任意宿主环境的自动继承。
- 独立临时 VM 没有选择代理，验证不能访问其槽位的 `10.204.1.1:7890`。该 VM 的 Rust TCP/UDP loopback 发布均到达 guest，撤销后 API 显示为空且 TCP 不再可达；最后正常 `guest-shutdown` 并删除实例。
- 管理服务停止后，代理和发布 mark 集合清空；更新管理程序并再次启动时，`cell-x1` 自动恢复 `ready`、代理和 `127.0.0.1:18787 -> 8787/tcp`。用户目录标记和 `/etc/ssh` 主机身份保持一致，代理 HTTPS 与 cells 功能复测通过。修复了工作负载先于管理 API 监听时立即失败的启动时序。
- `vmlinux`、`debian13.ext4`、initrd 的 inode 和 SHA-256 与升级前完全一致。仅保留正式的 `cell-x1` VM，临时测试均取得 temporary-process 完成与清理回执，临时密钥副本及验收凭据已删除。

旧 `smoke-network.py` 在 temporary-process 的用户命名空间内无法读取 VM 专用 UID 拥有的 jail 元数据，未宣称该完整脚本通过。本次实际网络/服务/端口检查改由有界作用域中的 API/SSH 驱动完成，不依赖该权限。外部入口 wildcard/netns 和管理服务 SIGKILL 的完整旧矩阵本次未重跑，历史结果见上节。

## 2026-11-07：管理 API 启动就绪与新镜像恢复

修复 `systemctl start firecracker-server` 返回后立即执行 `service up`/`list` 可能收到连接拒绝的问题：管理 unit 改为 `Type=notify`，初始化及 TCP bind 成功后由主进程发送 `READY=1`。工作负载复用同一通知实现。未运行 VM 的 SSH 地址查询补充 `VM_NOT_RUNNING` 上下文，CLI 日志阶段标识实际 runtime 子命令。

- Rust 49 项回归、Clippy、26 项构建安装回归、ShellCheck 和 Bash 语法通过；通知测试覆盖送达和失效 socket。
- 复现环境中，旧 cell-x1 已被用户销毁，管理服务已启动但 workload 绑定不存在。部署修复后连续执行管理服务启动、`service up cell-x1 cells-x1 --enable`、`list` 全部成功，没有插入 sleep；列表显示 `ready`、`clash`、`10.203.0.2` 和 `127.0.0.1:18787 -> 8787/tcp`。
- 新 cell-x1 使用新 Debian 基础镜像（202 个包），通过 SSH 实际检查 less/vim/fd/rg/top/ps/free/ss/ip/curl/jq/file/tree/lsof/ping/dig/unzip/rsync/pstree 全部可用。agent 用户执行 image-check 通过，Node/Codex/Claude/DSH 版本探测通过，cells 认证 capabilities 返回 serverId=cell-x1。
- SSH 验收驱动在 temporary-process 有界作用域内完成并取得成功清理回执；正式 cell-x1 保持 active/enabled。

## 2026-11-07：reboot 命令与 guest 服务文件保留

新增 `./vmm.sh reboot VM_ID` 和原生 `firecracker-server reboot VM_ID`。受管 VM 由 systemd restart 完整重启，保留开机启用状态；普通 VM 从 API 快照恢复 profile、目录、卷、代理及端口映射，持久根盘按已有策略复用。

服务文件默认只在缺失时写入，兼容没有投递记录的旧 VM，保留 guest 编辑的内容与权限。服务包 `[[files]]` 可显式设置 `overwrite = true`，表示每次启动由宿主覆盖；默认 false。缺失文件使用同目录临时文件和 hard link 原子提交，避免并发创建时覆盖用户文件；拒绝已有符号链接和非当前用户的目标。guest 准备事件及宿主日志记录写入/保留数量，不记录文件内容。

- Rust 57 项测试通过，包含普通 VM 重启策略/端口恢复、停止未确认/创建失败/无效快照、guest 编辑和包更新后保留、显式覆盖、旧 VM 迁移及符号链接拒绝；Clippy、26 项构建安装回归、ShellCheck、fmt、Bash 语法、diff 检查通过。
- 升级前确认 cell-x1 guest 配置已与宿主快照不同。新监督进程启动日志记录 `preserved=2, written=0`，没有回写旧快照。应用因用户新增的 prj 根导出与旧验证项 vm-write 子目录重叠而退出；保留用户的 prj/rw 及其 owner 授权，仅移除旧验证项的导出配置，未删除目录文件。
- 实际执行 `./vmm.sh reboot cell-x1` 成功，约 4122ms 后 ready；VM 实例标识改变，持久根盘 inode 相同，修正重叠项后的配置 SHA-256 在重启前后完全一致。cells 认证 capabilities/exports 验证通过，serverId=cell-x1，唯一导出为 prj/rw，代理和端口映射恢复，unit 保持 enabled。
- 普通 VM 的 API/SSH 验收驱动在 temporary-process 内未能调用控制程序，因为其用户命名空间无法验证宿主私有目录绑定；没有宣称该真实分支通过。所属临时 VM、持久根盘及密钥副本均已清理，驱动取得结束回执；普通 VM 分支本次由 Rust 顺序/失败测试覆盖。正式受管 cell-x1 的重启是实际运行结果。

## 2026-11-07 — virtio-fs 扩展目录元数据

定位 Cells 注册共享目录失败：宿主 ext4 提供 birth time，guest 原 virtiofsd 1.14.0 仅协商 FUSE 7.38，未实现 `FUSE_STATX`。新增可复现的源码补丁，协商 FUSE 7.39 并处理 STATX；按宿主 mask 返回创建时间，不合成缺失字段，保留 UID/GID 映射、句柄校验及只读包装转发。C 头文件核对 ABI：statx=256、input=24、output=288 字节，btime 偏移 80、响应 stat 偏移 32。

- virtiofsd release 测试：48 项单元测试、4 项文档测试通过，包含无 BTIME、无效 FD/句柄、非法 flags、UID/GID 映射、只读转发及协议布局。
- 构建/部署 Python 回归：28 项通过；外部 daemon 未声明 statx 时拒绝构建，不再静默复用旧系统 daemon。
- `./vmm.sh build` 通过，未请求 `--os`，日志确认跳过 kernel/rootfs；再次运行 daemon 构建复用 recipe/二进制摘要。
- 已原子替换安装的 daemon，并通过正式管理生命周期重启 `cell-x1`；原持久根盘和 18787→8787 映射保留，服务 ready/active。
- 真实 `cell-x1`：项目 `/home/li/share/prj/x1` 创建时间恢复；临时夹具的目录、子目录、普通文件、符号链接 birth time、inode、mode 与宿主一致。Cells `ro` 和 `rw` 项目注册、读取、解除注册通过；guest 写入在宿主可见。测试项目、源目录、临时 SSH known_hosts 和验证作用域均已清理。
- 通用 virtio-fs smoke 补充 rw/ro 创建时间检查；本次未重跑需停用正式管理服务的完整崩溃矩阵。

证据：`/var/tmp/fc-statx-vmm-build.log`、`/var/tmp/virtiofsd-statx-reuse.log`、`/var/tmp/virtiofsd-statx-pinned-tests.log`、`/var/tmp/fc-statx-build-regression-final.log`、`/var/tmp/fc-statx-deploy-reboot.log`、`/var/tmp/fc-statx-live-acceptance.log`、`/var/tmp/fc-statx-live-rw-acceptance.log`、`/var/tmp/fc-statx-live-receipt.json`。
