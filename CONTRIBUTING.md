# 贡献指南

欢迎提交可复现的问题、文档改进和范围明确的补丁。RDrive 使用 [MIT 许可证](LICENSE)；提交前确认你有权贡献相关内容，并保留第三方许可证及来源。

## 报告问题

在 [GitHub Issues](https://github.com/lorian7392/RDrive/issues) 中先搜索已有问题。新报告请包含：

- `rdrive --version`、源码提交（如可获得）、macOS 版本和 CPU 架构。
- 使用的入口：CLI 传输、原生挂载、Smart File Provider 或 MCP。
- 协议和可公开的服务端软件/版本；不要提供账户、内网完整地址或真实业务文件。
- 脱敏的命令、预期结果、实际结果、完整错误及最小复现步骤。
- 是否使用自定义状态/缓存目录、代理、后台服务，以及是否可在临时合成服务复现。

密码、token、私有状态目录和未公开漏洞按[安全说明](SECURITY.md)处理。不要将包含真实文件内容的上传快照作为附件。

## 准备开发环境

需要 Rust 1.90+、macOS 命令行开发工具和 Python 3.11+；原生测试还需要相应 Apple SDK / Swift 环境。先按[安装指南](docs/INSTALLATION.md)克隆并构建。

```sh
cargo build --locked
cargo test --locked
```

`Cargo.lock` 和 `vendor/` 补丁是复现的一部分，不要为普通功能修改顺手更新全部依赖。修改 vendored SMB 代码时，同时维护相关 `RDRIVE-PATCH.md`、上游来源和许可证。

测试必须使用专用临时状态目录、合成账户及测试存储，不能默认访问开发者真实云盘、NAS、挂载、钥匙串或代理配置。不要让旧版进程写入正在测试的升级后状态目录。

## 运行适合改动的检查

基础 Rust 检查之后，可按改动选择真实存在的黑盒工作流：

```sh
cargo build --locked
python3 tests/cli_workflows.py target/debug/rdrive
python3 tests/advanced_workflows.py target/debug/rdrive
python3 tests/cache_directory_workflows.py target/debug/rdrive
python3 tests/mcp_workflows.py target/debug/rdrive
python3 tests/index_workflows.py target/debug/rdrive
python3 tests/search_workflows.py target/debug/rdrive
python3 tests/policy_workflows.py target/debug/rdrive
python3 tests/service_workflows.py target/debug/rdrive
```

这些脚本有不同的覆盖范围，不表示每个补丁都要重复所有耗时流程。需要本机监听权限的测试应在允许其运行的环境执行。完整矩阵、报告参数及其余 Smart/命名/回执测试见[测试说明](docs/TESTING.md)。

SMB 与系统集成检查是独立层次：

```sh
python3 tests/smb_workflows.py target/debug/rdrive
python3 tests/mount_workflows.py target/debug/rdrive
python3 macos/tests/run.py
```

SMB fixture 需要可用 Docker；挂载、LaunchAgent 及实际 File Provider 工作流会调用系统功能，分别按测试说明准备隔离环境。原生系统测试还需构建并明确指定待测 App，不能用编译成功代替注册、下载、回调或 Finder 界面验证。

测试计数、工作流组数和代码覆盖率是不同指标。报告中明确列出通过、失败、ignored / skipped 及原因；没有执行的环境检查不能计为通过。只有实际生成并审查相应范围的覆盖率报告后，才能声称该范围的覆盖率；不要把“全部已运行测试通过”写成“100% 代码覆盖”。当前工具与口径以[测试说明](docs/TESTING.md)为准。

## 修改与测试原则

- 保留默认不覆盖、先预览后执行、版本条件、状态身份及路径范围保护。
- 新增恢复逻辑时验证失败、进程中断、响应丢失及并发外部修改；不能只测成功路径。
- 用真实请求/字节/状态断言支持行为，区分“已排队”“已发出”“服务器确认”和“系统已显示”。
- 保持改动集中，避免无关的全仓库格式化或依赖更新；遵循所在文件风格。
- 协议行为改变时更新功能说明、命令/配置参考和适用的工作流文档。
- 不改写历史 `docs/*VALIDATION*.json`、版本验证记录或既有证据来匹配新结果；新运行使用新的报告并注明版本及范围。

可用以下命令检查意外空白和未纳入版本控制的内容：

```sh
git diff --check
git status --short
```

构建输出、缓存、真实账户配置、`.runtime/`、`.env`、上传内容与临时测试数据不应进入补丁。

## 提交 Pull Request

说明具体触发问题、修改后的行为、为何这样处理，以及实际运行的检查及结果。列出未执行的相关环境检查和原因；如涉及状态迁移、覆盖、删除或恢复，说明兼容性与数据保留边界。

只附与本次改动有关、已脱敏的日志。文档示例使用保留的示例域名、通用账户名和占位路径，不复制个人环境路径或凭据。PR 中不要声称已发布、部署或生产验证，除非有对应证据。
