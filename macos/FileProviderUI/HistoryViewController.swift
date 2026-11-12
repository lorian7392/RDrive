import Foundation
import AppKit
import CryptoKit
import Darwin

func nativeHistoryError(_ error: Error) -> String {
    if let failure = error as? NativeFailure {
        switch failure { case .invalid(let message), .bridge(let message): return message }
    }
    return (error as NSError).localizedDescription
}

struct NativeHistoryEntry: Decodable {
    let id: String, item_id: String, profile: String, path: String, kind: String
    let reason: String, created: String, state: String, source: String
    let bytes: UInt64, files: UInt64
    let restorable: Bool
    var name: String { (path as NSString).lastPathComponent }
}

struct NativeHistoryPolicy: Codable {
    let enabled: Bool
    let keep: Int, max_age_days: Int
    let max_bytes: UInt64?
}

private struct NativeHistoryPage: Decodable {
    let item_id: String, path: String, kind: String
    let entries: [NativeHistoryEntry]
    let policy: NativeHistoryPolicy
    let total: Int, offset: Int
    let has_more: Bool
}

struct NativeHistoryIntegration {
    var request: ([String: Any], SocketOperation) throws -> [String: Any]
    var transfers: URL
    var saveURL: (String) -> URL? = NativeHistoryIntegration.chooseSaveURL
    var confirmReplace: (String) -> Bool = NativeHistoryIntegration.confirmReplacement
    var complete: () -> Void

    static func chooseSaveURL(_ name: String) -> URL? {
        let panel = NSSavePanel()
        panel.title = "Export Historical Copy"
        panel.nameFieldStringValue = name
        panel.canCreateDirectories = true
        return panel.runModal() == .OK ? panel.url : nil
    }
    static func confirmReplacement(_ message: String) -> Bool {
        let alert = NSAlert()
        alert.messageText = "Replace the current file?"
        alert.informativeText = "RDrive will preserve the current version and check for newer changes before replacing it.\n\n" + String(message.prefix(1500))
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Replace Current File")
        alert.addButton(withTitle: "Cancel")
        return alert.runModal() == .alertFirstButtonReturn
    }
}

/// Historical payloads are copies inside the authenticated private transfer
/// directory. Neither previews nor exports write through the live Finder item.
struct NativeHistoryPayload {
    struct Member: Decodable { let path: String, kind: String, bytes: UInt64; let sha256: String? }
    private struct Manifest: Decodable { let entries: [Member] }
    let url: URL, kind: String, bytes: UInt64
    let sha256: String?
    let members: [Member]
    private let device: dev_t, inode: ino_t

    init(_ reply: [String: Any], transfers: URL) throws {
        guard let path = reply["content_path"] as? String, path.hasPrefix("/"),
              let kind = reply["kind"] as? String, ["file", "directory"].contains(kind),
              let bytes = reply["bytes"] as? NSNumber, CFGetTypeID(bytes) != CFBooleanGetTypeID(), bytes.int64Value >= 0,
              reply["remote_changed"] as? Bool == false else { throw NativeFailure.invalid("The historical export response is incomplete.") }
        self.url = URL(fileURLWithPath: path).standardizedFileURL
        self.kind = kind; self.bytes = bytes.uint64Value; self.sha256 = reply["sha256"] as? String
        let root = transfers.resolvingSymlinksInPath().standardizedFileURL.path + "/"
        guard url.path == url.resolvingSymlinksInPath().path, url.path.hasPrefix(root) else {
            throw NativeFailure.invalid("The historical export is outside the private transfer directory.")
        }
        if kind == "directory" {
            guard reply["verified"] as? Bool == true, let manifest = reply["manifest"] else { throw NativeFailure.invalid("The directory export has no verified manifest.") }
            members = try JSONDecoder().decode(Manifest.self, from: JSONSerialization.data(withJSONObject: manifest)).entries
        } else { members = [] }
        var info = stat()
        guard lstat(url.path, &info) == 0 else { throw NativeFailure.invalid("The historical transfer copy disappeared.") }
        device = info.st_dev; inode = info.st_ino
        try verify(at: url)
    }
    static func relative(_ path: String) -> Bool {
        !path.isEmpty && !path.hasPrefix("/") && !path.contains("\0")
            && path.split(separator: "/", omittingEmptySubsequences: false).allSatisfy { !$0.isEmpty && $0 != "." && $0 != ".." }
    }
    static func fileHash(_ url: URL, size: UInt64) throws -> String {
        let descriptor = open(url.path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
        guard descriptor >= 0 else { throw NativeFailure.invalid("The historical content could not be opened safely.") }
        let file = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? file.close() }
        var info = stat()
        guard fstat(descriptor, &info) == 0, info.st_mode & S_IFMT == S_IFREG, info.st_uid == getuid(),
              info.st_nlink == 1, info.st_size >= 0, UInt64(info.st_size) == size else {
            throw NativeFailure.invalid("The historical content size or file identity changed.")
        }
        var hash = SHA256()
        while let data = try file.read(upToCount: 1024 * 1024), !data.isEmpty { hash.update(data: data) }
        return hash.finalize().map { String(format: "%02x", $0) }.joined()
    }
    func verify(at destination: URL) throws {
        let destination = destination.standardizedFileURL
        if kind == "file" {
            guard let sha256 = sha256, sha256.count == 64,
                  try Self.fileHash(destination, size: bytes) == sha256 else { throw NativeFailure.invalid("The historical content checksum does not match.") }
            return
        }
        let attributes = try FileManager.default.attributesOfItem(atPath: destination.path)
        guard attributes[.type] as? FileAttributeType == .typeDirectory else { throw NativeFailure.invalid("The historical directory changed type.") }
        var names = Set<String>(), total: UInt64 = 0
        for member in members {
            guard Self.relative(member.path), names.insert(member.path).inserted,
                  ["file", "directory"].contains(member.kind) else { throw NativeFailure.invalid("The historical directory manifest contains an unsafe path.") }
            let child = destination.appendingPathComponent(member.path)
            guard child.path == child.resolvingSymlinksInPath().path else { throw NativeFailure.invalid("The historical directory contains a symbolic link.") }
            if member.kind == "file" {
                let sum = total.addingReportingOverflow(member.bytes)
                guard !sum.overflow, let checksum = member.sha256,
                      try Self.fileHash(child, size: member.bytes) == checksum else { throw NativeFailure.invalid("A historical directory member failed verification.") }
                total = sum.partialValue
            } else {
                let attributes = try FileManager.default.attributesOfItem(atPath: child.path)
                guard attributes[.type] as? FileAttributeType == .typeDirectory else { throw NativeFailure.invalid("A historical directory member changed type.") }
            }
        }
        let actual = Set(try FileManager.default.subpathsOfDirectory(atPath: destination.path))
        guard actual == names, total == bytes else { throw NativeFailure.invalid("The historical directory contains missing or unrecorded content.") }
    }
    func preview() throws -> String {
        if kind == "directory" { return members.isEmpty ? "Empty folder." : members.prefix(200).map { ($0.kind == "directory" ? "Folder  " : "File    ") + $0.path }.joined(separator: "\n") + (members.count > 200 ? "\n…More entries are available in an exported copy." : "") }
        let file = try FileHandle(forReadingFrom: url); defer { try? file.close() }
        let data = try file.read(upToCount: 262144) ?? Data()
        guard let text = String(data: data, encoding: .utf8), !text.contains("\0") else { return "Binary file. Export a historical copy to inspect its contents." }
        return text + (bytes > 262144 ? "\n…Preview limited to 256 KiB. Export a copy for the complete file." : "")
    }
    func export(to destination: URL) throws {
        guard !FileManager.default.fileExists(atPath: destination.path) else { throw NativeFailure.invalid("Choose a new destination; the existing file was preserved.") }
        try FileManager.default.copyItem(at: url, to: destination)
        try verify(at: destination)
    }
    func removeStagedCopy() {
        var info = stat()
        if lstat(url.path, &info) == 0 && info.st_dev == device && info.st_ino == inode { try? FileManager.default.removeItem(at: url) }
    }
}

final class RDriveHistoryViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    let titleLabel = NSTextField(labelWithString: "File History")
    let message = NSTextField(wrappingLabelWithString: "Loading retained versions…")
    let table = NSTableView(), previewText = NSTextView()
    let previewButton = NSButton(title: "Preview", target: nil, action: nil)
    let exportButton = NSButton(title: "Export Copy…", target: nil, action: nil)
    let copyButton = NSButton(title: "Restore Copy", target: nil, action: nil)
    let replaceButton = NSButton(title: "Replace Current…", target: nil, action: nil)
    let previousButton = NSButton(title: "Previous", target: nil, action: nil), nextButton = NSButton(title: "Next", target: nil, action: nil)
    let enabled = NSButton(checkboxWithTitle: "Preserve history for this connection", target: nil, action: nil)
    let keep = NSTextField(string: "20"), days = NSTextField(string: "30"), space = NSTextField(string: "5120")
    let settingsButton = NSButton(title: "Save History Settings", target: nil, action: nil)
    var integration: NativeHistoryIntegration!
    private(set) var entries: [NativeHistoryEntry] = []
    private var context: (domain: String, id: String)?
    private var operation: SocketOperation?
    private var offset = 0, hasMore = false
    private var loadedSpace: (text: String, bytes: UInt64?) = ("5120", 5_368_709_120)

    override func loadView() {
        view = NSView(frame: NSRect(x: 0, y: 0, width: 820, height: 760))
        titleLabel.font = .boldSystemFont(ofSize: 18); message.isSelectable = true; message.maximumNumberOfLines = 3
        for (key, title, width) in [("created", "Saved", 160.0), ("reason", "Reason", 80), ("path", "Path", 220), ("bytes", "Size", 70), ("state", "State", 80), ("source", "Source", 90)] {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(key)); column.title = title; column.width = width; table.addTableColumn(column)
        }
        table.delegate = self; table.dataSource = self; table.allowsMultipleSelection = false
        let list = NSScrollView(); list.documentView = table; list.hasVerticalScroller = true; list.heightAnchor.constraint(equalToConstant: 190).isActive = true
        previewText.isEditable = false; previewText.isRichText = false
        let preview = NSScrollView(); preview.documentView = previewText; preview.hasVerticalScroller = true; preview.heightAnchor.constraint(equalToConstant: 140).isActive = true
        let actions = NSStackView(views: [previewButton, exportButton, copyButton, replaceButton])
        for (button, selector) in [(previewButton, #selector(showPreview)), (exportButton, #selector(exportCopy)), (copyButton, #selector(restoreCopy)), (replaceButton, #selector(replaceCurrent)), (previousButton, #selector(previousPage)), (nextButton, #selector(nextPage))] { button.target = self; button.action = selector }
        let paging = NSStackView(views: [previousButton, nextButton])
        let fields = NSStackView(views: [NSTextField(labelWithString: "Versions"), keep, NSTextField(labelWithString: "Days"), days, NSTextField(labelWithString: "MiB (blank = unlimited)"), space])
        for field in [keep, days, space] { field.widthAnchor.constraint(equalToConstant: 80).isActive = true }
        settingsButton.target = self; settingsButton.action = #selector(saveSettings)
        let done = NSButton(title: "Done", target: self, action: #selector(finish)); done.keyEquivalent = "\r"
        let stack = NSStackView(views: [titleLabel, message, list, paging, actions, preview, enabled, fields, settingsButton, done])
        stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 10; stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([stack.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20), stack.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -20), stack.topAnchor.constraint(equalTo: view.topAnchor, constant: 20), list.widthAnchor.constraint(equalTo: stack.widthAnchor), preview.widthAnchor.constraint(equalTo: stack.widthAnchor)])
        preferredContentSize = view.frame.size
        updateButtons()
    }
    func prepare(domain: String, id: String) {
        _ = view; context = (domain, id); entries = []; offset = 0; hasMore = false
        table.reloadData(); table.deselectAll(nil); previewText.string = ""; load(offset: 0)
    }
    func numberOfRows(in tableView: NSTableView) -> Int { entries.count }
    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard entries.indices.contains(row) else { return nil }
        let entry = entries[row]
        let value: String
        switch tableColumn?.identifier.rawValue {
        case "created": value = entry.created
        case "reason": value = entry.reason.capitalized
        case "path": value = entry.path
        case "source": value = entry.source == "local-history" ? "Local history" : entry.source
        case "bytes": value = ByteCountFormatter.string(fromByteCount: Int64(clamping: entry.bytes), countStyle: .file)
        default: value = entry.state.capitalized
        }
        let field = NSTextField(labelWithString: value); field.toolTip = value; return field
    }
    func tableViewSelectionDidChange(_ notification: Notification) { previewText.string = ""; updateButtons() }
    private var selected: NativeHistoryEntry? { entries.indices.contains(table.selectedRow) ? entries[table.selectedRow] : nil }
    private func updateButtons() {
        let ready = operation == nil && selected?.restorable == true
        previewButton.isEnabled = ready; exportButton.isEnabled = ready; copyButton.isEnabled = ready
        replaceButton.isEnabled = ready && selected?.kind == "file"
        previousButton.isEnabled = operation == nil && offset > 0; nextButton.isEnabled = operation == nil && hasMore
        settingsButton.isEnabled = operation == nil && context != nil
        enabled.isEnabled = operation == nil; for field in [keep, days, space] { field.isEnabled = operation == nil }
    }
    private func work(_ body: @escaping (SocketOperation) throws -> (() -> Void)) {
        operation?.cancel(); let current = SocketOperation(); operation = current; updateButtons()
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let update = try body(current)
                DispatchQueue.main.async { guard self.operation === current else { return }; self.operation = nil; update(); self.updateButtons() }
            } catch {
                DispatchQueue.main.async { guard self.operation === current else { return }; self.operation = nil; self.message.stringValue = "History operation stopped: " + nativeHistoryError(error); self.updateButtons() }
            }
        }
    }
    func load(offset requested: Int) {
        guard let context = context else { return }
        message.stringValue = "Loading retained versions…"
        work { operation in
            let reply = try self.integration.request(["op": "history_list", "domain": context.domain, "id": context.id, "offset": requested, "page_size": 100], operation)
            let page = try JSONDecoder().decode(NativeHistoryPage.self, from: JSONSerialization.data(withJSONObject: reply))
            guard page.item_id == context.id, page.offset == requested, page.total >= page.entries.count, page.entries.count <= 100,
                  Set(page.entries.map { $0.id }).count == page.entries.count,
                  page.entries.allSatisfy({ !$0.id.isEmpty && !$0.item_id.isEmpty && NativeHistoryPayload.relative($0.path) && ["file", "directory"].contains($0.kind) }) else { throw NativeFailure.invalid("The history listing does not match this selection.") }
            return {
                self.entries = page.entries; self.offset = page.offset; self.hasMore = page.has_more; self.previewText.string = ""; self.table.reloadData()
                self.titleLabel.stringValue = page.path.isEmpty ? "Connection History" : "History · " + page.path
                self.message.stringValue = page.total == 0 ? "No retained versions yet. Enable history to preserve future replacements and deletions." : "Showing \(page.offset + 1)–\(page.offset + page.entries.count) of \(page.total) retained versions."
                self.setPolicy(page.policy)
                if !page.entries.isEmpty { self.table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false) }
            }
        }
    }
    private func setPolicy(_ policy: NativeHistoryPolicy) {
        enabled.state = policy.enabled ? .on : .off; keep.stringValue = String(policy.keep); days.stringValue = String(policy.max_age_days)
        space.stringValue = policy.max_bytes.map { String($0 / 1_048_576 + ($0 % 1_048_576 == 0 ? 0 : 1)) } ?? ""
        loadedSpace = (space.stringValue, policy.max_bytes)
    }
    private func payload(_ entry: NativeHistoryEntry, context: (domain: String, id: String), operation: SocketOperation) throws -> NativeHistoryPayload {
        let reply = try integration.request(["op": "history_export", "domain": context.domain, "id": context.id, "version_id": entry.id], operation)
        return try NativeHistoryPayload(reply, transfers: integration.transfers)
    }
    @objc func showPreview() {
        guard operation == nil, let entry = selected, entry.restorable, let context = context else { return }
        message.stringValue = "Reading a verified historical copy…"
        work { operation in let payload = try self.payload(entry, context: context, operation: operation); defer { payload.removeStagedCopy() }; let text = try payload.preview(); return { self.previewText.string = text; self.message.stringValue = "Read-only historical preview. The current file is unchanged." } }
    }
    @objc func exportCopy() {
        guard operation == nil, let entry = selected, entry.restorable, let context = context, let destination = integration.saveURL(entry.name) else { return }
        message.stringValue = "Exporting a verified historical copy…"
        work { operation in let payload = try self.payload(entry, context: context, operation: operation); defer { payload.removeStagedCopy() }; try payload.export(to: destination); return { self.message.stringValue = "Exported a historical copy to " + destination.path } }
    }
    private func restore(mode: String) {
        guard operation == nil, let entry = selected, entry.restorable, let context = context else { return }
        message.stringValue = "Checking the restore destination…"
        work { operation in
            let plan = try self.integration.request(["op": "history_restore_prepare", "domain": context.domain, "id": context.id, "version_id": entry.id, "mode": mode], operation)
            guard plan["can_restore"] as? Bool == true, let planID = plan["plan_id"] as? String, !planID.isEmpty, plan["mode"] as? String == mode, let target = plan["target"] as? String else { throw NativeFailure.invalid("RDrive did not provide a guarded restore plan.") }
            return {
                if mode == "replace" && !self.integration.confirmReplace((plan["message"] as? String ?? "") + "\nDestination: " + target) { self.message.stringValue = "Restore cancelled. The current file is unchanged."; return }
                self.work { operation in
                    let result = try self.integration.request(["op": "history_restore", "domain": context.domain, "id": context.id, "version_id": entry.id, "plan_id": planID], operation)
                    guard result["restored"] as? Bool == true, let path = result["path"] as? String else { throw NativeFailure.invalid("RDrive did not confirm that the historical copy was restored.") }
                    return { self.message.stringValue = "Restored historical content at " + path }
                }
            }
        }
    }
    @objc func restoreCopy() { restore(mode: "copy") }
    @objc func replaceCurrent() { if selected?.kind == "file" { restore(mode: "replace") } }
    @objc func previousPage() { if offset > 0 { load(offset: max(0, offset - 100)) } }
    @objc func nextPage() { if hasMore { load(offset: offset + 100) } }
    @objc func saveSettings() {
        guard operation == nil, let context = context else { return }
        guard let count = Int(keep.stringValue), count > 0, let age = Int(days.stringValue), age > 0 else { message.stringValue = "Enter positive whole numbers for versions and days."; return }
        var bytes: Any = NSNull()
        if space.stringValue == loadedSpace.text {
            bytes = loadedSpace.bytes.map { $0 as Any } ?? NSNull()
        } else if !space.stringValue.isEmpty {
            guard let mib = UInt64(space.stringValue), mib > 0 else { message.stringValue = "Enter a positive space limit in MiB, or leave it blank."; return }
            let value = mib.multipliedReportingOverflow(by: 1_048_576)
            guard !value.overflow else { message.stringValue = "The history space limit is too large."; return }
            bytes = value.partialValue
        }
        let policy: [String: Any] = ["enabled": enabled.state == .on, "keep": count, "max_age_days": age, "max_bytes": bytes]
        work { operation in
            let reply = try self.integration.request(["op": "history_config", "domain": context.domain, "history_policy": policy], operation)
            guard reply["saved"] as? Bool == true, let saved = reply["policy"] else { throw NativeFailure.invalid("RDrive did not confirm the history settings.") }
            let policy = try JSONDecoder().decode(NativeHistoryPolicy.self, from: JSONSerialization.data(withJSONObject: saved))
            return { self.setPolicy(policy); self.message.stringValue = "History settings saved for this connection. Existing pending edits are retained." }
        }
    }
    func cancel() { operation?.cancel(); operation = nil }
    @objc func finish() { cancel(); integration.complete() }
    deinit { operation?.cancel() }
}
