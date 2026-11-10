# 托管仓库与跨进程崩溃事务

托管仓库把文件内容保存为不可变对象，把目录、文件稳定身份和选定的标签/扩展属性保存为完整快照。一个事务可以同时添加、替换、移动和删除多个逻辑文件；提交前读者看到旧快照，提交成功后看到完整的新快照。普通远端目录的 `put`、`sync` 和系统 SMB/WebDAV 挂载不自动获得这套事务语义。

## 使用

先创建一个专用远端目录；已有普通文件不会自动导入仓库：

```sh
rdrive mkdir dav managed
rdrive repo init dav managed
rdrive repo begin dav managed
# 从输出复制事务 id，下面用 <TX> 表示。
rdrive repo mkdir <TX> documents
rdrive repo put <TX> ./report.pdf documents/report.pdf
rdrive repo put <TX> ./notes.txt notes.txt
rdrive repo status <TX>
rdrive repo commit <TX>
rdrive repo ls dav managed
rdrive repo get dav documents/report.pdf ./downloaded.pdf --root managed
```

后续事务支持 `repo mv <TX> FROM TO`、`repo rm <TX> PATH --recursive`。本地源内容在 `repo put` 时复制并校验；之后修改源文件不会改变已暂存的事务。`repo transactions` 列出本机持久记录。

提交失败、进程退出、或客户端没收到成功回执时，保留原状态目录，然后重新运行 **同一个** `repo commit <TX>`。程序根据不可变提交槽和事务身份核对已完成结果；已经发布的事务不会生成第二次提交。`repo abort <TX>` 仅允许尚未准备提交的事务；准备过的事务可能已经对其他客户端可见，必须先核对提交结果。

两个客户端从同一快照开始时，只有一个能提交到下一个序号；另一个报告冲突，保留暂存内容。不会自动合并或覆盖获胜客户端的更新。重新读取最新快照、检查冲突并建立新事务后才能重新应用修改。

## 发布与恢复规则

- 控制目录为 `.rdrive-repository/`，包含 `FORMAT.json`、`objects/`、`commits/` 和 `staging/`。
- 内容路径由 SHA-256 决定。发布前完整下载校验内容对象与暂存清单，再以服务器禁止覆盖的重命名发布下一个提交槽。
- 提交槽按序号递增并包含前一提交的校验和；客户端检查完整父链、内容绑定、稳定 UUID、父目录和大小写/Unicode 名称冲突。
- 不使用可被旧客户端覆盖的可变 HEAD，也不依赖容易在崩溃后残留的远端锁。并发提交竞争同一个不可变槽。
- 本机 `transactions/` 保存阶段记录，`spool/` 保存源快照。发布完成但本机记录尚未更新时，重放会从服务器的提交身份恢复。
- File Provider 的版本检查、内容读取和新事务绑定到同一快照。快照读取后另一个客户端提交，不会导致旧版本标记配上新文件字节，也不会悄悄覆盖新的提交。

这保证的是**客户端进程崩溃后的原子可见性**，前提是服务器提供正确的同文件系统原子重命名和禁止覆盖语义。没有声称验证过服务器断电、磁盘控制器缓存、损坏/恶意服务器或绕过仓库协议直接修改控制文件的恢复保证。

## File Provider 连接

```sh
rdrive provider domain-add managed-domain dav managed --repository --display-name 'Managed Documents'
rdrive provider domains
```

再按 [macOS 扩展说明](MACOS_INTEGRATION.md) 注册相同 domain。通过这个逻辑视图才能在 Finder 中看到仓库文件名和原子快照；直接用系统网络挂载查看该远端目录会看到内部存储结构。

托管 File Provider 支持稳定身份的文件/目录创建、文件内容及选定元数据更新、移动、重命名和删除。普通目录模式的安全能力较少，尤其不提供受版本保护的目录删除和重命名。

## 当前边界

没有垃圾回收、历史压缩、自动迁移、分支合并或自动冲突合并。旧提交、不可达内容、失败暂存数据和本机源快照会占用空间；不要把控制目录当作普通缓存清理。当前读取验证完整提交历史，提交验证所有引用对象，大仓库开销会随历史和内容增长。

文件标签和允许的扩展属性随快照原子发布。目录标签、资源叉、ACL、隔离标记和安全属性不在此元数据范围内。本机多扩展属性更新另有恢复日志，macOS 本身不提供跨多个属性的原子系统调用。

File Provider 本地索引和重放记录绑定到连接协议、地址、用户名、远端根目录和仓库模式。复用旧 domain ID 却改指向其他存储会拒绝操作并保留记录；应为新存储创建新 domain ID。
