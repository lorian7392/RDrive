# rdrive 0.3.0 验证记录

日期：2026-11-09。环境：macOS 27.2、Apple Silicon arm64、Rust/Cargo 1.90.0、Swift 6.4.2。原生扩展使用当前 CLT SDK 构建。

所有服务端测试使用本机临时 WebDAV 或隔离的加密 Samba 4.21.9 容器，账户与文件均为合成数据；没有使用真实 NAS 或云盘账户。0.2 原生挂载和旧功能的详细结果保留在 [历史验证记录](VALIDATION_0.2.md)。下面记录 0.3 当前新增验证；最终交付校验信息见 `dist/build-info.json`，核心结构化结果见 [CORE_VALIDATION.json](CORE_VALIDATION.json)。

## 核心与网络回归

最终完整 Rust 回归通过 **101 项**：72 项库测试、3 项公开挂载 API 测试、26 项实际 HTTP 协议测试。另有 4 项依赖原生挂载或 Samba 的测试单独运行。

完整测试覆盖本地路径/目录锚定、同步冲突与恢复副本、下载检查点、真实 macOS 标签和扩展属性、元数据旁车修订绑定、托管事务、桥接路径隔离、共享限速以及实际 WebDAV 请求。需要原生挂载或 Samba 服务的测试明确单独运行，不能把 ignored 计为通过。

托管事务故障测试覆盖：发布前中断仍只见旧快照；发布成功但成功回执丢失；实际任务在发布后被取消；不同状态目录的两个客户端竞争同一提交槽；损坏提交链与 Unicode/大小写冲突；已固定快照读取不混入新版本字节；过时快照的事务不覆盖其他客户端的新提交。

0.3 发布版 WebDAV 命令行流程通过 **9 组**，SMB 命令行流程通过 **10 组**（包括托管仓库的实际双客户端提交竞争）；原生加密 SMB 只读/可写挂载均验证了真实服务端内容、重命名、删除和正常卸载，WebDAV 原生只读挂载也通过。

实际 HTTP 黑盒流程检查：

- 选定的 Finder 标签和 `user.*` 属性往返、元数据独立同步和清空。
- 远端仅修改内容时保留本地标签；新修订带明确空元数据记录时正确清除标签。
- 三个上传任务真实重叠执行，完成后逐字节核对。
- 64KiB 文件在共享 32KiB/s 限速下至少耗时 1.7 秒；JSON 进度记录实际已传输字节。
- 托管多文件提交、读者可见性、重放和两个独立客户端竞争；服务器发布提交但尚未回执时，实际 SIGKILL 客户端进程，新进程重放同一事务恢复相同校验和且不生成新提交。
- 认证 TCP 桥接、无认证请求拒绝、只枚举不下载文件内容、托管 CRUD/重放/移动、相同内容但不同路径的稳定身份，以及内容保存不丢标签。额外验证 16 个并发系统回调全部排队成功、同名创建冲突、删除回执丢失后同路径出现替代文件、快照读取与过时修改的受控交错。

## SMB 实际协议与故障测试

隔离 Samba 使用强制 SMB3 加密。生命周期测试实际验证 CHANGE_NOTIFY、取消、通知队列满时取消、溢出/断连后的重扫与重新订阅；读取 RH lease 授予、lease break 和认证 ACK；durable-v2 授予、TCP 断开后的 DH2C 重连。

服务端重启后用重新认证的受保护打开继续；身份或内容变化会拒绝续传。上传会重新验证临时文件身份、真实长度、全部已有前缀和发布收据；注入 TCP 断连、服务端重启、认证失败及前缀篡改后分别核对结果。占位枚举仅使用元数据接口，实测文件正文读取为零。

[SMB 结构化验证记录](SMB_VALIDATION.json) 保存发布文件 SHA-256、十组工作流和恢复诊断。当前 0.3 测试程序的生命周期、受保护覆盖、跨进程检查点恢复三个真实服务测试分别通过（约 11.66 秒、0.43 秒和 13.86 秒）。普通路径覆盖仍保留两次重命名之间的路径空缺；原子目录可见性来自新增托管仓库，不应混为一谈。没有验证服务器断电、CA persistent handle、跨进程持久保存 durable token、脏写缓存或字节范围锁。

## 原生 Finder / File Provider

原生 SDK/主机验证通过 **11 组**，包括标签归档转换、合法原始属性的字节保持、Unicode、颜色、清空和无效格式拒绝。最终发布版还通过 **8 组真实 File Provider 工作流**：零内容 GET 的系统 dataless 占位文件、精确内容按需下载、驱逐、自动离线保留与禁止驱逐、取消保留后重新驱逐/下载、原生标签及 `#S` 用户扩展属性写入/删除、创建/受版本保护的修改/删除，以及停掉后端仍可读取已下载文件。

实际编译、签名、注册、操作系统占位文件、下载/驱逐与 Finder UI 的逐项结果见 [MACOS_INTEGRATION.md](MACOS_INTEGRATION.md)。不能把编译或注册成功当成所有原生交互已经验证。

Finder UI 实际显示 File Provider 的 Keep Offline / Allow Automatic Removal 自定义操作；点击取消保留后，独立桥接查询确认 `pinned=false`。单独的 Finder Sync 扩展在 CloudStorage 和显式映射的普通测试目录都未显示自定义菜单/徽标。系统日志记录了扩展启动与 Finder 握手后取消上下文；尚未确定显示失败根因，不能把构建或开关状态视为渲染成功。

当前开发路径使用显式私有目录和带随机认证令牌的本机 TCP。App Group 生产路径需要具有授权 Team 的签名；本机 ad-hoc 构建不能声明获得生产 App Group 权限或完成公证。

## 复现

```sh
cargo test --locked
python3 tests/cli_workflows.py target/release/rdrive
python3 tests/advanced_workflows.py target/release/rdrive
python3 tests/smb_workflows.py target/release/rdrive
python3 tests/mount_workflows.py target/release/rdrive
python3 macos/tests/run.py
python3 tests/provider_native_workflows.py --binary target/release/rdrive
```

SMB 故障证明需要 `tests/smb_workflows.py` 中的 `RDRIVE_SMB_TEST_RUNNER`、`RDRIVE_SMB_PROOF_ONLY`，生命周期另加 `RDRIVE_SMB_LIFECYCLE=1`。产品运行不依赖 Docker，只有 Samba 测试 fixture 使用它。原生 fixture 的具体参数以脚本帮助为准；需要系统允许应用/扩展测试。

本次 Cargo 使用项目参数指定缓存镜像，不修改全局配置：

```sh
cargo --config 'source.crates-io.replace-with="rsproxy"' \
  --config 'source.rsproxy.registry="sparse+https://rsproxy.cn/index/"' \
  test --locked --offline
```

SMB 0.12.1 的两个 vendored 依赖及 MIT 许可证、来源、补丁范围分别见 [smb 补丁](../vendor/smb/RDRIVE-PATCH.md) 和 [smb-msg 补丁](../vendor/smb-msg/RDRIVE-PATCH.md)。当前补丁包含通知/取消、lease 路由和 durable reconnect 支持，不再只有 ShareAccess 修改。源码、依赖和产物 SHA-256 收录于发布构建记录。

## 限制

真实 NAS、多厂商 WebDAV/SMB、长时间弱网、高并发性能和生产签名安装尚未验证。普通路径 `cp/mv` 不自动迁移元数据旁车；选定元数据同步适用于常规文件，目录标签、资源叉、隔离/安全属性和 ACL 不在范围内。托管仓库保留历史和暂存内容，尚无垃圾回收。桥接服务会为并发的系统回调排队，当前普通 File Provider 网络操作仍串行确认状态；并行传输由 CLI 队列的 `--jobs` 提供。其他独立 CLI 进程仍可能在状态目录忙时被拒绝，需要稍后重试。磁盘缓存和源快照为明文。
