# rdrive 0.3.1 验证记录

日期：2026-11-09。环境：macOS 27.2、Apple Silicon、Rust 1.90.0。此前 0.3.0 完整功能与原生 UI 验证保留在 [0.3.0 历史记录](VALIDATION_0.3.0.md) 和 [历史结构化结果](CORE_VALIDATION_0.3.0.json)。

## 本次修复

某些 WebDAV 服务为目录返回空或纯空白的 `getcontentlength`。旧解析器立即按整数解析，导致目录枚举失败。现在等完整资源类型与成功属性合并后再判断：目录可省略大小，普通文件仍必须有可靠的非负长度。缺失、空值、畸形和混合空值的文件大小均会拒绝，真实零字节文件正常接受。

客户端新增 reqwest 的 macOS 系统代理支持。此前关闭 reqwest 默认特性时，也关闭了系统代理自动发现；现在可自动采用系统 HTTP/HTTPS 代理。保持原有 Rustls 证书校验和禁止重定向策略。

Cargo.lock 新增 system-configuration 0.7.0、system-configuration-sys 0.6.0 和 core-foundation 0.9.4，保留原有 core-foundation 0.10.1。没有升级或移除其他依赖。

## 当前验证

共 **103 项 Rust 测试通过**：72 项库测试、28 项真实 HTTP 协议集成测试、3 项公开挂载 API 测试。4 项依赖特定服务或挂载环境的测试按原设置单独运行。新回归先在旧解析器复现原始错误，再验证修复；覆盖属性先后顺序、多个成功 propstat、目录空大小、零字节文件和普通文件非法大小。

本次经用户授权接入真实的 123 云盘 WebDAV：

- 先前目标命中 Stash 的 GEOIP/CN 直连规则。直连路径未能建立目标 HTTPS；使用已存在的东京、悉尼和香港出口时，证书校验通过并收到正常认证提示。
- 保持同一目标 IP/SNI，绑定物理网卡的直连连接也超时；经过东京出口则完成 TLS。没有证据需要调整 MTU。
- 仅为该完整 WebDAV 域名新增一条精确路由，先备份源配置并验证候选只多这一条。运行配置与候选哈希一致，手动选择组及 C1/C2/C3 保留。
- 系统内核确认实际 `webdav` 挂载、精确来源和当前用户；读写标记为 `readonly=false`。
- 最终 0.3.1 可在不设置 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY 环境变量时完成真实账号认证，读取 22 个目录项目；与系统挂载列出的名称完全一致。
- 验证范围为认证、目录读取和系统读写标记；没有对真实账户执行文件写入、删除或重命名测试。密码未保存到配置、命令参数、报告或源码。

个人挂载路径、路由备份与连接状态保存在本地忽略的私有 `.runtime/`，不进入交付包。真实账户保持用户要求的挂载状态。

## 原生包与历史验证范围

原生包更新为 0.3.1/build4，内含最新 CLI，重新通过深度严格代码签名校验。仍是显式私有 IPC 路径的 ad-hoc 开发包，尚无生产签名/公证。

0.3.0 的 10 组 SMB 工作流、11 组原生主机测试、8 组真实 File Provider 流程和 Finder UI 记录属于历史验证，本次没有将这些结果冒充为针对新产物重新执行。自定义 FinderSync 菜单/徽标的显示限制继续适用。

## 复现与证据

```sh
cargo test --locked
python3 tests/cli_workflows.py target/release/rdrive
python3 tests/advanced_workflows.py target/release/rdrive
```

本次使用项目参数指定 rsproxy 镜像，未修改全局 Cargo 配置。结构化结果见 [CORE_VALIDATION.json](CORE_VALIDATION.json)，交付文件及源码校验和见 `dist/build-info.json` 和 `dist/SHA256SUMS`。

Stash 诊断采用官方说明的[按请求选择出口](https://stash.wiki/en/script/syntax-and-interface)及[运行规则/连接查询](https://stash.wiki/en/features/http-api)，以实际连接链和真实挂载结果确认修复。
