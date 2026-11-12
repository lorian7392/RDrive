import Foundation
import AppKit
import FileProvider
import CryptoKit
import ObjectiveC
import Darwin

private final class HistorySDKMasks {
    var records: [(Method, IMP, IMP)] = []
    func replace(_ type: AnyClass, _ selector: String, block: Any) throws {
        guard let method = class_getInstanceMethod(type, NSSelectorFromString(selector)) else { throw TestFailure(message: "Missing dialog selector") }
        let replacement = imp_implementationWithBlock(block)
        records.append((method, method_setImplementation(method, replacement), replacement))
    }
    deinit { for (method, original, replacement) in records.reversed() { method_setImplementation(method, original); imp_removeBlock(replacement) } }
}

/// Real SDK view objects and filesystem verification; only peer replies and
/// modal OS boundaries are replaced inside this disposable test process.
enum HistoryCoverageTests {
    static let policy: [String: Any] = ["enabled": true, "keep": 20, "max_age_days": 30, "max_bytes": 5_368_709_120]
    static func entry(_ changes: [String: Any] = [:]) -> [String: Any] {
        var value: [String: Any] = ["id": "old-1", "item_id": "item", "profile": "synthetic", "path": "Folder/file.txt", "kind": "file", "reason": "overwrite", "created": "2026-11-12T12:00:00Z", "bytes": 5, "files": 1, "state": "confirmed", "restorable": true, "source": "local-history"]
        for (key, item) in changes { value[key] = item }; return value
    }
    static func page(_ entries: [[String: Any]], offset: Int = 0, total: Int? = nil, more: Bool = false, path: String = "Folder/file.txt", policy: [String: Any] = policy) -> [String: Any] {
        ["item_id": "item", "path": path, "kind": "file", "entries": entries, "policy": policy, "total": total ?? entries.count, "offset": offset, "has_more": more]
    }
    static func sha(_ bytes: Data) -> String { SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined() }
    static func file(_ paths: NativePaths, bytes: Data = Data("hello".utf8)) throws -> [String: Any] {
        let url = paths.transfers.appendingPathComponent(UUID().uuidString)
        try bytes.write(to: url)
        return ["content_path": url.path, "kind": "file", "bytes": bytes.count, "sha256": sha(bytes), "remote_changed": false]
    }
    static func folder(_ paths: NativePaths, count: Int = 1) throws -> [String: Any] {
        let url = paths.transfers.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        var entries: [[String: Any]] = []
        if count > 0 {
            let child = url.appendingPathComponent("empty")
            try FileManager.default.createDirectory(at: child, withIntermediateDirectories: false)
            entries.append(["path": "empty", "kind": "directory", "bytes": 0])
            for i in 0..<count {
                let name = "file-\(i)"; let bytes = Data("hello".utf8)
                try bytes.write(to: url.appendingPathComponent(name))
                entries.append(["path": name, "kind": "file", "bytes": bytes.count, "sha256": sha(bytes)])
            }
        }
        return ["content_path": url.path, "kind": "directory", "bytes": count * 5, "sha256": String(repeating: "f", count: 64), "remote_changed": false, "verified": true, "manifest": ["entries": entries]]
    }
    static func run(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        try expect(nativeHistoryError(NativeFailure.bridge("offline")) == "offline", "Bridge reason hidden")
        try expect(nativeHistoryError(NSError(domain: "TestHistory", code: 2, userInfo: [NSLocalizedDescriptionKey: "System failure"])) == "System failure", "System reason hidden")
        try payloads(paths)
        try dialogs(paths)
        try controller(paths)
        try status(paths, bridge)
        print("PASS native history list/settings, immutable preview/export, guarded restore and dialog boundaries")
    }
    static func payloads(_ paths: NativePaths) throws {
        let fm = FileManager.default
        func reject(_ value: [String: Any], _ label: String) throws { try UICoverageTests.reject(label) { _ = try NativeHistoryPayload(value, transfers: paths.transfers) } }
        try reject([:], "Empty export response accepted")
        var response = try file(paths)
        for (key, bad): (String, Any) in [("bytes", true), ("bytes", -1), ("remote_changed", true), ("kind", "link"), ("content_path", "relative")] {
            var badResponse = response; badResponse[key] = bad; try reject(badResponse, "Invalid payload field \(key)")
        }
        let payload = try NativeHistoryPayload(response, transfers: paths.transfers)
        try expect(try payload.preview() == "hello", "Text history preview failed")
        let exported = paths.group.appendingPathComponent(UUID().uuidString)
        try payload.export(to: exported)
        try expect(try Data(contentsOf: exported) == Data("hello".utf8), "History export changed bytes")
        try UICoverageTests.reject("History export clobbered a destination") { try payload.export(to: exported) }
        for badHash in ["bad", String(repeating: "0", count: 64)] { var bad = response; bad["sha256"] = badHash; try reject(bad, "Bad hash accepted") }
        var bad = response; bad["bytes"] = 6; try reject(bad, "Wrong size accepted")
        bad = response; bad["content_path"] = paths.group.appendingPathComponent("missing").path; try reject(bad, "Outside transfer accepted")
        bad["content_path"] = paths.transfers.appendingPathComponent("missing").path; try reject(bad, "Missing file accepted")
        let linked = paths.transfers.appendingPathComponent(UUID().uuidString)
        try fm.createSymbolicLink(at: linked, withDestinationURL: payload.url)
        bad = response; bad["content_path"] = linked.path; try reject(bad, "Symlink accepted")
        try UICoverageTests.reject("No-follow hash followed a symlink") { _ = try NativeHistoryPayload.fileHash(linked, size: 5) }
        let hard = paths.transfers.appendingPathComponent(UUID().uuidString)
        try fm.linkItem(at: payload.url, to: hard); try reject(response, "Hardlinked history accepted"); try fm.removeItem(at: hard)
        let replacement = paths.transfers.appendingPathComponent(UUID().uuidString)
        try Data("new inode".utf8).write(to: replacement)
        payload.removeStagedCopy(); payload.removeStagedCopy()
        try expect(!fm.fileExists(atPath: payload.url.path), "Staging payload was not cleaned")
        try fm.moveItem(at: replacement, to: payload.url); payload.removeStagedCopy()
        try expect(fm.fileExists(atPath: payload.url.path), "Cleanup removed a changed staging inode")
        let binary = try NativeHistoryPayload(file(paths, bytes: Data([0, 255])), transfers: paths.transfers)
        try expect(try binary.preview().contains("Binary file"), "Binary preview was decoded as text")
        let long = try NativeHistoryPayload(file(paths, bytes: Data(repeating: 65, count: 262145)), transfers: paths.transfers)
        try expect(try long.preview().contains("256 KiB"), "Preview byte bound missing")
        let empty = try NativeHistoryPayload(file(paths, bytes: Data()), transfers: paths.transfers)
        try expect(try empty.preview().isEmpty, "Empty preview failed")
        let directory = try NativeHistoryPayload(folder(paths), transfers: paths.transfers)
        try expect(try directory.preview().contains("Folder  empty"), "Directory preview missing")
        let directoryCopy = paths.group.appendingPathComponent(UUID().uuidString)
        try directory.export(to: directoryCopy)
        let emptyDirectory = try NativeHistoryPayload(folder(paths, count: 0), transfers: paths.transfers)
        try expect(try emptyDirectory.preview() == "Empty folder.", "Empty directory preview failed")
        let many = try NativeHistoryPayload(folder(paths, count: 201), transfers: paths.transfers)
        try expect(try many.preview().contains("More entries"), "Directory preview was unbounded")
        response = try folder(paths)
        bad = response; bad.removeValue(forKey: "manifest"); try reject(bad, "Unverified directory accepted")
        bad = response; bad["verified"] = false; try reject(bad, "False verification accepted")
        let manifest = (response["manifest"] as! [String: Any])["entries"] as! [[String: Any]]
        for unsafePath in ["", "/outside", "../escape", "a//b", "a/./b", "a/../b", "a\0b"] {
            var value = manifest; value[1]["path"] = unsafePath; bad = response; bad["manifest"] = ["entries": value]; try reject(bad, "Unsafe relative member accepted")
        }
        for change in [["kind": "unknown"], ["sha256": "bad"], ["sha256": NSNull()], ["path": "empty", "kind": "file"]] as [[String: Any]] {
            var value = manifest; for (key, data) in change { value[1][key] = data }; bad = response; bad["manifest"] = ["entries": value]; try reject(bad, "Invalid member accepted")
        }
        bad = response; bad["manifest"] = ["entries": manifest + [manifest[0]]]; try reject(bad, "Duplicate member accepted")
        bad = response; bad["manifest"] = ["entries": Array(manifest.dropFirst())]; try reject(bad, "Unrecorded directory accepted")
        bad = response; bad["bytes"] = 6; try reject(bad, "Directory total mismatch accepted")
        let root = URL(fileURLWithPath: response["content_path"] as! String)
        try fm.removeItem(at: root.appendingPathComponent("empty")); try Data().write(to: root.appendingPathComponent("empty"))
        try reject(response, "File replacing directory member accepted")
        try fm.removeItem(at: root.appendingPathComponent("empty")); try fm.createSymbolicLink(at: root.appendingPathComponent("empty"), withDestinationURL: paths.group)
        try reject(response, "Directory member symlink accepted")
        try UICoverageTests.reject("Directory root changed type accepted") { try directory.verify(at: exported) }
        try UICoverageTests.reject("Missing hash input accepted") { _ = try NativeHistoryPayload.fileHash(paths.transfers.appendingPathComponent("absent"), size: 0) }
    }
    static func dialogs(_ paths: NativePaths) throws {
        let masks = HistorySDKMasks()
        var accepted = true
        let modal: @convention(block) (AnyObject) -> Int = { _ in accepted ? NSApplication.ModalResponse.OK.rawValue : NSApplication.ModalResponse.cancel.rawValue }
        let url: @convention(block) (AnyObject) -> NSURL? = { _ in paths.group.appendingPathComponent("export-dialog.txt") as NSURL }
        try masks.replace(NSSavePanel.self, "runModal", block: modal)
        try masks.replace(NSSavePanel.self, "URL", block: url)
        try expect(NativeHistoryIntegration.chooseSaveURL("old.txt")?.lastPathComponent == "export-dialog.txt", "Save panel adapter lost accepted URL")
        accepted = false; try expect(NativeHistoryIntegration.chooseSaveURL("old.txt") == nil, "Save panel cancel accepted")
        let alert: @convention(block) (AnyObject) -> Int = { _ in accepted ? NSApplication.ModalResponse.alertFirstButtonReturn.rawValue : NSApplication.ModalResponse.alertSecondButtonReturn.rawValue }
        try masks.replace(NSAlert.self, "runModal", block: alert)
        try expect(!NativeHistoryIntegration.confirmReplacement("test"), "Replace alert cancel ignored")
        accepted = true; try expect(NativeHistoryIntegration.confirmReplacement(String(repeating: "x", count: 2000)), "Replace alert confirm ignored")
        withExtendedLifetime(masks) {}
    }
    static func controller(_ paths: NativePaths) throws {
        let controller = RDriveHistoryViewController()
        var seen: [[String: Any]] = [], currentPage = page([entry()], total: 101, more: true)
        var fail: String?, invalidPlan = false, invalidResult = false, confirm = true, done = 0, cancelSave = false, invalidSave = false
        var savedPolicy: [String: Any] = policy
        let destination = paths.group.appendingPathComponent(UUID().uuidString)
        controller.integration = NativeHistoryIntegration(request: { request, _ in
            seen.append(request)
            let op = request["op"] as! String
            if fail == op { throw NativeFailure.invalid("Synthetic history failure") }
            switch op {
            case "history_list": return currentPage
            case "history_export": return try file(paths)
            case "history_restore_prepare": return invalidPlan ? [:] : ["plan_id": "plan", "mode": request["mode"]!, "target": "Folder/restored.txt", "can_restore": true, "message": "Preserve current"]
            case "history_restore": return invalidResult ? [:] : ["restored": true, "path": "Folder/restored.txt"]
            case "history_config": savedPolicy = request["history_policy"] as! [String: Any]; return invalidSave ? [:] : ["saved": true, "policy": savedPolicy]
            default: throw TestFailure(message: "Unknown history operation")
            }
        }, transfers: paths.transfers, saveURL: { name in try? expect(name == "file.txt", "Export filename wrong"); return cancelSave ? nil : destination }, confirmReplace: { message in try? expect(message.contains("Destination"), "Replace destination omitted"); return confirm }, complete: { done += 1 })
        _ = controller.view
        controller.load(offset: 0); controller.showPreview(); controller.exportCopy(); controller.restoreCopy(); controller.replaceCurrent(); controller.saveSettings(); controller.previousPage(); controller.nextPage()
        controller.prepare(domain: "synthetic", id: "item")
        try UICoverageTests.pump("History list not loaded") { controller.entries.count == 1 && controller.nextButton.isEnabled }
        try expect(controller.numberOfRows(in: controller.table) == 1 && controller.copyButton.isEnabled && controller.replaceButton.isEnabled, "History controls unavailable")
        for column in controller.table.tableColumns { try expect(controller.tableView(controller.table, viewFor: column, row: 0) is NSTextField, "History table column missing") }
        try expect(controller.tableView(controller.table, viewFor: nil, row: -1) == nil, "Invalid history table row accepted")
        _ = controller.tableView(controller.table, viewFor: nil, row: 0)
        controller.showPreview(); controller.showPreview(); controller.saveSettings()
        try UICoverageTests.pump("History preview not loaded") { controller.previewText.string == "hello" }
        try expect(controller.message.stringValue.contains("unchanged"), "Preview did not report read-only state")
        cancelSave = true; controller.exportCopy(); cancelSave = false; controller.exportCopy()
        try UICoverageTests.pump("History export did not finish") { controller.message.stringValue.contains("Exported") }
        try expect(try Data(contentsOf: destination) == Data("hello".utf8), "Controller export changed historical bytes")
        controller.exportCopy(); try UICoverageTests.pump("No-clobber error not shown") { controller.message.stringValue.contains("existing file") }
        controller.restoreCopy(); try UICoverageTests.pump("Restore copy did not complete") { controller.message.stringValue.hasPrefix("Restored") }
        try expect(seen.contains { $0["mode"] as? String == "copy" }, "Restore default was not copy")
        confirm = false; controller.replaceCurrent(); try UICoverageTests.pump("Replace cancellation not displayed") { controller.message.stringValue.contains("Restore cancelled") }
        let commits = seen.filter { $0["op"] as? String == "history_restore" }.count
        confirm = true; controller.replaceCurrent(); try UICoverageTests.pump("Guarded replacement not completed") { controller.message.stringValue.hasPrefix("Restored") }
        try expect(seen.filter { $0["op"] as? String == "history_restore" }.count == commits + 1 && seen.last?["plan_id"] as? String == "plan", "Replacement lost guarded plan")
        invalidPlan = true; controller.restoreCopy(); try UICoverageTests.pump("Invalid plan accepted") { controller.message.stringValue.contains("guarded restore plan") }; invalidPlan = false
        invalidResult = true; controller.restoreCopy(); try UICoverageTests.pump("Invalid restore receipt accepted") { controller.message.stringValue.contains("did not confirm") }; invalidResult = false
        fail = "history_export"; controller.showPreview(); try UICoverageTests.pump("Export error hidden") { controller.message.stringValue.contains("Synthetic history") }; fail = nil
        controller.keep.stringValue = "0"; controller.saveSettings(); try expect(controller.message.stringValue.contains("positive whole"), "Invalid keep accepted")
        controller.keep.stringValue = "20"; controller.days.stringValue = "-1"; controller.saveSettings(); controller.days.stringValue = "30"
        controller.space.stringValue = "bad"; controller.saveSettings(); try expect(controller.message.stringValue.contains("positive space"), "Invalid MiB accepted")
        controller.space.stringValue = String(UInt64.max); controller.saveSettings(); try expect(controller.message.stringValue.contains("too large"), "MiB overflow accepted")
        for value in ["1", "", ""] {
            controller.space.stringValue = value; controller.saveSettings()
            try UICoverageTests.pump("History settings not saved") { controller.message.stringValue.hasPrefix("History settings saved") && controller.settingsButton.isEnabled }
            try expect(value.isEmpty ? savedPolicy["max_bytes"] is NSNull : (savedPolicy["max_bytes"] as? UInt64 == 1_048_576), "Saved MiB policy incorrect")
        }
        invalidSave = true; controller.saveSettings(); try UICoverageTests.pump("Invalid settings acknowledgment accepted") { controller.message.stringValue.contains("did not confirm") }; invalidSave = false
        currentPage = page([entry()], offset: 100, total: 101, more: false)
        controller.nextPage(); try UICoverageTests.pump("Next history page failed") { controller.previousButton.isEnabled && !controller.nextButton.isEnabled }
        currentPage = page([entry()], more: false, policy: ["enabled": false, "keep": 3, "max_age_days": 4, "max_bytes": 1])
        controller.previousPage(); try UICoverageTests.pump("Previous history page failed") { !controller.previousButton.isEnabled && controller.space.stringValue == "1" }
        controller.saveSettings(); try UICoverageTests.pump("Exact fractional MiB policy not preserved") { controller.settingsButton.isEnabled && controller.message.stringValue.hasPrefix("History settings saved") }
        try expect(savedPolicy["max_bytes"] as? UInt64 == 1, "Unedited fractional MiB limit rounded")
        currentPage = page([entry(["kind": "directory", "restorable": false])], path: "")
        controller.load(offset: 0); try UICoverageTests.pump("Unrestorable directory not loaded") { controller.titleLabel.stringValue == "Connection History" && !controller.copyButton.isEnabled }
        controller.showPreview(); controller.exportCopy(); controller.restoreCopy(); controller.replaceCurrent()
        currentPage = page([]); controller.load(offset: 0); try UICoverageTests.pump("Empty history missing") { controller.message.stringValue.contains("No retained versions") }
        currentPage = page([entry(["path": "../escape"])]); controller.load(offset: 0); try UICoverageTests.pump("Unsafe history listing accepted") { controller.message.stringValue.contains("does not match") }
        controller.finish(); try expect(done == 1, "History completion missing")
        // A detached older operation cannot update a new selection or error.
        for throwsOld in [false, true] {
            let started = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0), ended = DispatchSemaphore(value: 0)
            controller.integration.request = { _, _ in started.signal(); release.wait(); defer { ended.signal() }; if throwsOld { throw NativeFailure.invalid("Old response") }; return page([entry()]) }
            controller.prepare(domain: "synthetic", id: "item"); try expect(started.wait(timeout: .now() + 2) == .success, "Old request not started")
            controller.cancel(); let before = controller.message.stringValue; release.signal(); try expect(ended.wait(timeout: .now() + 2) == .success, "Old request did not finish")
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.03)); try expect(controller.message.stringValue == before, "Stale history callback changed current state")
        }
    }
    static func status(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        let controller = RDriveStatusViewController(nibName: nil, bundle: nil)
        var domain: NSFileProviderDomainIdentifier? = NSFileProviderDomainIdentifier("synthetic-history"), fail = false, done = 0
        var integration = NativeStatusIntegration(makeBridge: { if fail { throw NativeFailure.invalid("History bridge unavailable") }; return bridge }, readItem: { _, _, _ in try NativeTests.fixtureItem() }, domain: { _ in domain }, manager: { _ in nil }, identify: { _, reply in reply(nil, nil, nil) }, complete: { _ in done += 1 })
        integration.request = { requestBridge, request, _ in try expect(requestBridge.paths.group == paths.group && request["op"] as? String == "history_list", "History authentication context changed"); return page([entry()]) }
        controller.integration = integration
        func details() -> String { (controller.view.subviews.compactMap { $0 as? NSStackView }.first!.arrangedSubviews[1] as! NSTextField).stringValue }
        controller.prepare(forAction: "rdrive.history", itemIdentifiers: [])
        try expect(details().contains("Select one"), "History accepted empty selection")
        domain = nil; controller.prepare(forAction: "rdrive.history", itemIdentifiers: [providerID("item")]); try expect(details().contains("Select one"), "History accepted absent domain"); domain = NSFileProviderDomainIdentifier("synthetic-history")
        controller.prepare(forAction: "rdrive.history", itemIdentifiers: [providerID("item")])
        try UICoverageTests.pump("Status history child not attached") { controller.historyController?.entries.count == 1 }
        try expect(controller.preferredContentSize.width == 820, "History sheet did not resize")
        controller.historyController?.finish(); try expect(done == 1, "History Done did not finish extension")
        controller.prepare(forError: NativeFailure.invalid("New status error")); try expect(controller.historyController == nil && controller.preferredContentSize.width == 480, "History child not detached")
        fail = true; controller.prepare(forAction: "rdrive.history", itemIdentifiers: [providerID("item")]); try UICoverageTests.pump("History bridge error hidden") { details().contains("History unavailable") }
        // Cancelling each possible completion prevents late history attachment.
        for shouldFail in [false, true] {
            let started = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0), ended = DispatchSemaphore(value: 0)
            controller.integration.makeBridge = { started.signal(); release.wait(); defer { ended.signal() }; if shouldFail { throw NativeFailure.invalid("old bridge") }; return bridge }
            controller.prepare(forAction: "rdrive.history", itemIdentifiers: [providerID("item")]); try expect(started.wait(timeout: .now() + 2) == .success, "History bridge not entered")
            controller.prepare(forAction: "unknown", itemIdentifiers: []); release.signal(); try expect(ended.wait(timeout: .now() + 2) == .success, "Old bridge did not finish")
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.03)); try expect(controller.historyController == nil && details().contains("Select RDrive"), "Late history callback changed current action")
        }
    }
}
