# 测试、覆盖率与复现

RDrive 的检查分为 Rust 行为测试、隔离协议工作流、macOS 宿主/mock 测试和真实系统验收。
测试全部通过不等于全部代码已执行；mock 通过也不等于系统已接受扩展或 Finder 已显示菜单。
历史结果保留在[版本验证记录](README.md#版本验证记录)，不能作为当前源码的覆盖率。

本次冻结源码的逐语言 100% 结果与逐文件哈希见[生产覆盖率验收](COVERAGE.md)。后续改动需要重新采集。

## 环境与隔离

- Rust 1.90+，使用提交的 `Cargo.lock` 和 `vendor/` 补丁。
- 完整开发测试使用 Python 3.11+；固定的 `coverage==7.17.2` 要求 Python 3.11 或更新版本。普通 Python 测试使用标准库。
- macOS 原生测试需要 Xcode 命令行工具、Swift、FileProvider、FinderSync 和 AppKit SDK。
- DAV/IPC 工作流需允许临时回环 HTTP/TCP/Unix socket；不访问真实服务。
- Samba 工作流需要 Docker 和 fixture 镜像/软件包可用。容器、账户和共享为临时合成数据，退出时清理。

测试使用临时状态目录和合成密码，不读取真实钥匙串、不更改代理或现有连接。
`macos/tests/run.py` 默认把产物放到 `target/native-tests`，不会覆盖 `macos/build/RDrive.app`。
原生 mock 测试可以构造离屏控件，但不显示窗口、不运行应用事件循环、不注册/删除真实域、不请求真实内容驱逐。

## 快速检查

在仓库根目录运行：

```sh
cargo build --locked
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --check
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --mcp --check
python3 macos/tests/run.py
```

文档测试默认读取 `target/debug/rdrive`；可以设置 `RDRIVE_TEST_BINARY` 指定另一个待测可执行文件。
CLI 参考检查独立枚举源码命令与运行时帮助，并校验配置示例、原生参数、文档链接及生成器错误处理。
Python 脚本测试通过 mock 编译器、签名工具及子进程，核对真实临时包结构、plist、哈希、文件权限与回滚。

Rust 部分默认 `ignored` 的 Samba proof 需要下述 fixture，不能把 ignored 计为通过。
开发用的 AST 范围工具另测：

```sh
cargo test --locked --manifest-path tools/coverage-scope/Cargo.toml
```

## 隔离行为工作流

以下脚本都接收待测 CLI 路径。没有 Docker 的基础 DAV/IPC 检查：

```sh
python3 tests/cli_workflows.py target/debug/rdrive
python3 tests/advanced_workflows.py target/debug/rdrive
python3 tests/smart_workflows.py target/debug/rdrive
python3 tests/namespace_workflows.py target/debug/rdrive
python3 tests/provider_receipts_workflows.py target/debug/rdrive
python3 tests/coverage_cli_workflows.py target/debug/rdrive
python3 tests/cache_directory_workflows.py target/debug/rdrive --report target/validation/cache.json
python3 tests/mcp_workflows.py target/debug/rdrive --report target/validation/mcp.json
python3 tests/index_workflows.py target/debug/rdrive --output target/validation/index.json
python3 tests/search_workflows.py target/debug/rdrive --report target/validation/search.json
python3 tests/policy_workflows.py target/debug/rdrive --output target/validation/policy.json
python3 tests/service_workflows.py target/debug/rdrive --output target/validation/service.json
```

这些工作流验证实际请求、字节、持久状态、进程终止和独立写者竞争。
报告要注明二进制版本/哈希，不能将一版程序的结果归给另一版。
没有报告路径选项的脚本会输出结果，部分另生成被 Git 忽略的 `docs/*.debug.json`；不要覆盖提交的版本证据。

SMB 全流程与原始/Smart bridge 分开运行：

```sh
python3 tests/smb_workflows.py target/debug/rdrive
RDRIVE_SMB_PROVIDER_ONLY=1 python3 tests/smb_workflows.py target/debug/rdrive
RDRIVE_SMB_PROVIDER_ONLY=1 RDRIVE_SMB_SMART=1 \
  python3 tests/smb_workflows.py target/debug/rdrive
```

`RDRIVE_DOCKER` 可以指定 Docker 可执行文件。
`RDRIVE_SMB_TEST_RUNNER` 指向已编译的 Rust 库测试可执行文件时，fixture 还执行 ignored 的 guarded/resumable proof。
设置 `RDRIVE_SMB_LIFECYCLE=1 RDRIVE_SMB_PROOF_ONLY=1` 则在该 fixture 中单独验证 lifecycle proof。
这些变量只用于测试脚本，不是客户端生产配置。

## 功能与测试导航

- 连接、路径与后端能力：`tests/coverage_backend.rs`、`tests/webdav.rs`、`src/config.rs` 测试。
- 下载/检查点、不可变上传快照、进度、限速与并行：`src/resume.rs`、`src/operations.rs`、`src/transfer.rs`、`src/scheduler.rs`，以及 CLI/advanced/SMB 工作流。
- 缓存预算、固定、活跃读者、清理、目录迁移：`tests/cache.rs`、`tests/cache_directory_workflows.py`、`src/cache*.rs`。
- 普通同步与元数据：`src/sync.rs`、`src/metadata*.rs`、CLI/advanced/SMB 工作流。
- 托管事务和响应丢失：`src/repository.rs`、advanced 工作流；含多写者竞争与发布后 SIGKILL。
- Smart 队列、命名操作、冲突决定：`tests/smart.rs`、`src/provider/`、Smart/namespace/policy/receipts 工作流。
- SQLite、增量目录发现与搜索：`src/provider/index*.rs`、index/search 工作流。
- MCP：`src/mcp.rs`、`src/mcp/catalog.rs`、`tests/mcp_catalog_contract.rs` 和 MCP stdio 工作流；包含权限、范围、协议和取消。
- 挂载与服务：`tests/mount.rs` 的假系统、service 工作流；实际系统检查另列。
- Swift：`macos/tests/NativeTests.swift` 和 `coverage_*.swift`；真实 SDK 数据对象配合可注入 bridge、manager、通知和界面边界。
- 全命令/参数契约：`src/main.rs` 的 `cli_contract` 遍历真实 Clap 命令树，逐项验证全部 97 个叶命令、727 个含位置参数和继承项的参数声明、枚举值、数值类型及冲突参数。当前产生 1,034 个合法值解析用例，并把清单写入 `target/validation/cli-parameter-cases.json`。
- MCP 字段契约：`tests/mcp_catalog_contract.rs` 验证全部 81 个工具、186 个字段的模式、合法/非法输入、位置/选项映射和真实 CLI 帮助解析；当前有 394 个合法用例和 2,392 个非法用例。帮助解析不会执行存储命令或创建状态目录。
- 全参数文档：`tests/test_documentation.py` 核对帮助树、配置和原生参数；生成参考文档的 788 项选项声明还包含帮助/版本等自动项和分组，因此与上面的生产参数契约计数口径不同。契约不代表所有参数组合的业务语义都已执行；业务行为由对应模块和协议工作流验证。

可以单独复验上述契约：

```sh
cargo test --locked --bin rdrive cli_contract::
cargo test --locked --test mcp_catalog_contract
python3 -m unittest discover -s tests -p 'test_documentation.py' -v
```

## 测量生产代码行覆盖率

验收目标为**每种语言分别达到 100% 生产可执行行覆盖率**，不能用语言合计掩盖某种语言的缺口。
范围是 `src/` 的当前目标 Rust、全部 macOS 生产 Swift、四个分发 Python 脚本：
`macos/build.py`、`scripts/install-skills.py`、`scripts/mcp-config.py`、`scripts/generate-cli-docs.py`。
依赖、测试、生成物和开发测试工具不计入应用范围；生产适配器、错误、恢复和平台路径均保留。

Rust/Swift 取 LLVM LCOV 的唯一 `(文件, 行)` 执行记录。同一行因闭包、泛型或多个对象重复出现时取最大命中数；
保留原始报告，不能直接把重复计数的 Swift `LF`/LLVM summary 当作物理源码行数。
Rust 内联测试范围由 `syn` 解析语法树，只剔除必然要求 `cfg(test)` 的项目及测试函数，不用正则猜测括号。
未被测量的生产源文件使验收失败，不能当成零行省略。
稳定 Rust 行插桩未收集分支覆盖，分支状态为“未测量”，不能写成 100%。

Rust 工具安装和完整采集：

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.1 --locked
python3 tests/source_manifest.py --output target/coverage/source-manifest.json
python3 tests/run_rust_coverage.py \
  --output-dir target/coverage/rust-acceptance --workflows all
```

`--output-dir` 必须尚不存在，确保 raw profiles 全新。收集器从 `cargo-llvm-cov show-env` 读取插桩环境，
用 Cargo JSON 锁定本次编译的 CLI 和所有 Rust 测试可执行文件；先运行普通测试，再以同一个 CLI 运行本页全部 DAV/IPC/Samba 工作流，
包括三个 fixture 内实际执行的 ignored SMB proof。真实网络挂载测试仍保持 ignored，不计为通过。
最后仅使用这些明确列出的对象调用 Rust 自带 LLVM，输出 `coverage.lcov`、`test-spans.json`、每条命令日志和 `manifest.json`。
manifest 保存生产源码与对象的哈希；采集前后源码/对象变化、命令失败或缺少 profiles 都会使采集失败。
这些检查不依赖 Cargo 覆盖率工具对旧目录的自动对象发现。

`--target-dir PATH` 可以复用构建缓存，仍使用全新 profiles 和本次 Cargo 返回的准确对象；`--offline` 禁止 Cargo 联网。
`--cargo-llvm-cov PATH` 可指定已安装的工具。`--workflows dav` 只采集 DAV/IPC，`smb` 只采集 Samba，`none` 只运行 Rust 测试；
这些子集本身不等于完整验收。`--test-suite portable-smb` 只运行 SMB 库测试和 operations 集成测试，且必须配合 `--workflows smb`；
它用于 Linux 的补充协议测试，macOS 验收必须保留默认 `all`。
不要在汇总前清理采集目录、删除 `.profraw` 或替换对象。修改源码后重新采集，不混算旧行号。

Swift 与 Python 独立采集（Apple 和 Rust LLVM 版本可能不同，不混合各自 raw profiles）：

```sh
python3 macos/tests/run.py --coverage --output-dir target/native-tests-coverage
python3 -m venv target/python-coverage-venv
target/python-coverage-venv/bin/python -m pip install coverage==7.17.2
COVERAGE_FILE=target/coverage/python.coverage \
RDRIVE_TEST_BINARY="$PWD/target/coverage/rust-acceptance/build/debug/rdrive" \
target/python-coverage-venv/bin/python -m coverage run --branch \
  --include='*/macos/build.py,*/scripts/*.py' \
  -m unittest discover -s tests -p 'test_*.py'
COVERAGE_FILE=target/coverage/python.coverage \
target/python-coverage-venv/bin/python -m coverage json -o target/coverage/python.json
python3 tests/source_manifest.py --output target/coverage/source-manifest.json --check
```

统一报告与严格门槛：

```sh
python3 tests/report_coverage.py \
  --rust-lcov target/coverage/rust-acceptance/coverage.lcov \
  --rust-test-spans target/coverage/rust-acceptance/test-spans.json \
  --swift-lcov target/native-tests-coverage/coverage.lcov \
  --python-json target/coverage/python.json \
  --source-manifest target/coverage/rust-acceptance/manifest.json \
  --source-manifest target/native-tests-coverage/coverage-manifest.json \
  --source-manifest target/coverage/source-manifest.json \
  --output target/coverage/production.json
```

默认 `--min-line 100`，正式验收要求每个生产源文件都有当前匹配的来源哈希清单；上面的源清单在测试前生成，测试后复验。
任何语言不足、缺少映射、清单不匹配或输入不完整都会返回失败，并保留每文件哈希和未覆盖行。
诊断时可显式指定较低门槛，但该运行不能作为“100% 验收通过”的证据。
多个 Python 分批结果可重复 `--python-json` 合并，按实际命中取并集，不叠加重复行。

## 持续集成门槛

`.github/workflows/checks.yml` 把普通 Rust 测试、DAV/IPC、Swift 和 Python 检查放在 macOS，
把需要 Docker 的 Samba fixture 放在 Linux。两个 Rust 收集器在同一 Git 版本上各自冻结对象与源码。
最终任务校验全部源码哈希，以第一个 macOS 收集结果的可执行行清单定义目标范围，
让 Linux 的真实 SMB 执行只补充**相同 macOS 源码行**的命中；Linux 专属平台行不属于 macOS 覆盖率，原始 Linux 报告仍保留。
这验证 macOS 客户端的生产行覆盖率，不能描述为 Linux 客户端达到 100%。

合并使用经过测试的 `tests/merge_rust_coverage.py`，参数顺序必须先 macOS、后 Linux：

```sh
python3 tests/merge_rust_coverage.py \
  --input-dir target/coverage/rust-macos \
  --input-dir target/coverage/rust-linux \
  --output target/coverage/rust.lcov
```

第一个输入必须来自 macOS、运行全部普通测试及 DAV 工作流；任何版本、源码清单或哈希不匹配均拒绝合并。
收集器也在 manifest 绑定 LCOV 的 SHA-256；报告被误替换后即使源码未变化，合并仍会拒绝。
最终 `report_coverage.py` 对 Rust、Swift、Python 分别执行默认 100% 门槛，任一不足都会使 CI 失败。
原始 LCOV、源码/对象清单、工作流日志和最终逐文件缺口报告作为构建产物保存；本地测试通过不代表 GitHub 上的任务已运行或通过。

## 实际系统验收另行执行

`tests/mount_workflows.py` 会实际挂载/卸载测试网络卷；`service_workflows.py --launchd` 会操作临时用户 LaunchAgent。
`tests/provider_native_workflows.py` 和 `macos/tests/activation_probe.py` 涉及真实扩展、注册域及系统状态。
它们不能作为无副作用的基础 CI，须阅读脚本、明确待测 App/域/IPC，在独立测试账户或专用环境运行。
不应把默认开发者 IPC 路径用于与正在运行的个人域并发测试。

真实 Finder 菜单、状态图标、占位显示、下载、驱逐、离线编辑和标签显示分别需要系统观察证据。
mock 覆盖证明客户端逻辑按预期调用接口；不证明服务器断电恢复、所有 SMB/WebDAV 实现兼容或所有 macOS 版本可用。
