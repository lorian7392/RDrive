import Foundation
import FileProvider
import UniformTypeIdentifiers
import Darwin

struct TestFailure: Error { let message: String }
func expect(_ value: @autoclosure () throws -> Bool, _ message: String) throws {
    if try !value() { throw TestFailure(message: message) }
}
func wait(_ semaphore: DispatchSemaphore) throws {
    try expect(semaphore.wait(timeout: .now() + 5) == .success, "Native callback timed out")
}

final class EnumerationProbe: NSObject, NSFileProviderEnumerationObserver {
    let done = DispatchSemaphore(value: 0)
    var items: [NSFileProviderItem] = []
    var next: NSFileProviderPage?
    var error: Error?
    var suggestedPageSize: Int { 1 }
    func didEnumerate(_ updatedItems: [NSFileProviderItem]) { items += updatedItems }
    func finishEnumerating(upTo nextPage: NSFileProviderPage?) { next = nextPage; done.signal() }
    func finishEnumeratingWithError(_ error: Error) { self.error = error; done.signal() }
}
final class ChangesProbe: NSObject, NSFileProviderChangeObserver {
    let done = DispatchSemaphore(value: 0)
    var items: [NSFileProviderItem] = []
    var deleted: [NSFileProviderItemIdentifier] = []
    var anchor: NSFileProviderSyncAnchor?
    var error: Error?
    var suggestedBatchSize: Int { 1 }
    func didUpdate(_ updatedItems: [NSFileProviderItem]) { items += updatedItems }
    func didDeleteItems(withIdentifiers deletedItemIdentifiers: [NSFileProviderItemIdentifier]) { deleted += deletedItemIdentifiers }
    func finishEnumeratingChanges(upTo anchor: NSFileProviderSyncAnchor, moreComing: Bool) { self.anchor = anchor; done.signal() }
    func finishEnumeratingWithError(_ error: Error) { self.error = error; done.signal() }
}

@main
enum NativeTests {
    static func testFinderTagNormalization() throws {
        func archive(_ object: [String: Any]) throws -> Data {
            try NSKeyedArchiver.archivedData(withRootObject: object, requiringSecureCoding: true)
        }
        func reject(_ data: Data, _ message: String) throws {
            do { _ = try nativeFinderTagData(data); throw TestFailure(message: message) }
            catch is NativeFailure { }
        }
        try expect(try nativeFinderTagData(nil) == nil, "Missing tag value did not clear")
        try expect(try nativeFinderTagData(Data()) == nil, "Empty tag value did not clear")
        let raw = try PropertyListSerialization.data(fromPropertyList: ["Blue\n4", "自定义\n0"], format: .xml, options: 0)
        try expect(try nativeFinderTagData(raw) == raw, "Canonical raw tag bytes changed")
        let known = try archive(["v": 1, "t": [["Blue", 4], ["自定义", 0], ["Gray", 7]]])
        let normalized = try nativeFinderTagData(known)!
        let strings = try PropertyListSerialization.propertyList(from: normalized, options: [], format: nil) as? [String]
        try expect(strings == ["Blue\n4", "自定义\n0", "Gray\n7"], "Native archive names or colors changed")
        try expect(try nativeFinderTagData(archive(["v": 1, "t": []])) == nil, "Empty native tag array did not clear")
        let invalid: [[String: Any]] = [
            ["v": 2, "t": []], ["v": true, "t": []], ["v": 1, "t": [], "extra": 1],
            ["v": 1, "t": [[1, 4]]], ["v": 1, "t": [["", 4]]],
            ["v": 1, "t": [["A\n4", 2]]], ["v": 1, "t": [["A\rB", 2]]], ["v": 1, "t": [["A\0B", 2]]],
            ["v": 1, "t": [["Blue", true]]], ["v": 1, "t": [["Blue", 1.5]]],
            ["v": 1, "t": [["Blue", -1]]], ["v": 1, "t": [["Blue", 8]]]
        ]
        for object in invalid { try reject(archive(object), "Invalid native tag archive accepted") }
        try reject(PropertyListSerialization.data(fromPropertyList: ["unknown": 1], format: .binary, options: 0), "Unknown tag format accepted")
        let largeTags = (0..<100).map { String(repeating: "x", count: 800) + String($0) + "\n4" }
        try reject(PropertyListSerialization.data(fromPropertyList: largeTags, format: .xml, options: 0), "Oversized raw tags accepted")
        let largePairs: [[Any]] = (0..<100).map { [String(repeating: "x", count: 800) + String($0), 4] }
        try reject(archive(["v": 1, "t": largePairs]), "Oversized normalized tags accepted")
        try reject(Data(repeating: 0, count: 1024 * 1024 + 1), "Oversized tag archive accepted")
        for tags in [[""], ["bad\0tag"], [String(repeating: "x", count: 1031)], Array(repeating: "Blue\n4", count: 1025)] {
            try reject(PropertyListSerialization.data(fromPropertyList: tags, format: .binary, options: 0), "Invalid raw tag array accepted")
        }
        try reject(archive(["v": 1, "t": [["Blue"]]]), "Incomplete native tag pair accepted")
        try reject(archive(["v": 1, "t": [[String(repeating: "x", count: 1025), 4]]]), "Overlong native tag name accepted")
        try reject(archive(["v": 1, "t": Array(repeating: ["Blue", 4], count: 1025)]), "Too many native tags accepted")
        print("PASS native tag archive normalization, exact raw bytes, explicit clear, names/colors and size bounds")
    }

    static func rejects(_ message: String, _ body: () throws -> Void) throws {
        do { try body() } catch { return }
        throw TestFailure(message: message)
    }

    static func fixtureItem(_ changes: [String: Any] = [:]) throws -> WireItem {
        var raw: [String: Any] = ["id": "test-file", "parent_id": "root", "name": "test.txt", "kind": "file", "size": 19,
            "content_version": "content", "metadata_version": "metadata", "pinned": false, "capabilities": ["read"]]
        raw.merge(changes) { _, right in right }
        return try WireItem.decode(raw)
    }

    static func testValidation(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        let object: [String: Any] = ["path": paths.group.path, "domain": "fixture"]
        let root = try JSONDecoder().decode(WatchedRoot.self, from: JSONSerialization.data(withJSONObject: object))
        try expect(root.domain == "fixture" && root.path == paths.group.path, "Watched roots did not round trip")
        try expect(paths.roots.lastPathComponent == "finder-roots.json", "Root mapping path changed")
        for id in ["root", "working_set", "trash", "stable-file"] {
            try expect(wireID(providerID(id)) == id, "Special provider identity did not round trip")
        }
        for name in ["", "user.bad\n#S", "com.apple.metadata:_kMDItemUserTags#S", "user.no-suffix", "com.apple.quarantine#S"] {
            try expect(!nativeSyncableAttribute(name), "Unsafe native attribute accepted")
        }
        try expect(nativeSyncableAttribute("com.apple.metadata:test#S"), "Safe metadata attribute refused")
        try expect(try nativeAttributePayload(["user.test#S": Data([0, 255]), "security.test#S": Data([1])]) == ["user.test#S": "00ff"], "Attribute encoding/filtering failed")
        try rejects("Oversized outgoing attributes accepted") { _ = try nativeAttributePayload(["user.test#S": Data(repeating: 0, count: 32769)]) }
        try expect(nativeHexData("0") == nil && nativeHexData("zz") == nil, "Invalid hex accepted")
        try expect(nativeHexData("") == Data() && nativeHexData("01FF") == Data([1, 255]), "Hex decoder changed valid bytes")
        for changes: [String: Any] in [["name": ""], ["name": "."], ["name": ".."], ["name": "a/b"], ["name": "a\0b"], ["id": ""], ["kind": "symlink"], ["metadata_version": String(repeating: "x", count: 129)], ["extended_attributes": ["user.test#S": "0"]], ["extended_attributes": ["user.test#S": "GG"]], ["extended_attributes": ["user.test#S": String(repeating: "00", count: 32769)]]] {
            try rejects("Invalid wire metadata accepted") { _ = try fixtureItem(changes) }
        }
        for state in NativeSyncState.allCases {
            try expect(!state.title.isEmpty, "State has no display title")
        }
        try expect(NativeSyncState.item(fixtureItem(["sync_state": "unknown", "pinned": true])) == .pinned, "Unknown state did not preserve pin fallback")
        try expect(NativeSyncState.item(fixtureItem()) == .online, "Default state changed")
        let codes: [String: NSFileProviderError.Code] = ["no_such_item": .noSuchItem, "not_authenticated": .notAuthenticated,
            "filename_collision": .filenameCollision, "sync_anchor_expired": .syncAnchorExpired, "version_conflict": .cannotSynchronize, "excluded_by_filter": .cannotSynchronize,
            "insufficient_quota": .insufficientQuota, "directory_not_empty": .directoryNotEmpty, "deletion_rejected": .deletionRejected,
            "unknown": .serverUnreachable]
        for (code, expected) in codes { try expect(remoteError(["code": code]).code == expected.rawValue, "Typed backend error changed") }
        try expect(remoteError([:]).code == NSFileProviderError.serverUnreachable.rawValue, "Missing backend code was not conservative")
        try expect(remoteError(["code": "cancelled"]).code == NSUserCancelledError, "Cancellation error mapping changed")
        try expect(remoteError(["code": "unsupported"]).code == NSFeatureUnsupportedError, "Unsupported error mapping changed")
        let cocoa = NSError(domain: NSCocoaErrorDomain, code: 1)
        try expect(providerError(cocoa) === cocoa, "Existing Cocoa error was replaced")
        try expect(providerError(NativeFailure.invalid("synthetic")).code == NSFileProviderError.serverUnreachable.rawValue, "Untyped native error did not map")
        let progress = Progress(totalUnitCount: 7)
        let operation = SocketOperation(progress: progress)
        for (completed, total): (Int64, Int64) in [(-1, 10), (11, 10), (0, 0)] { operation.report(completed: completed, total: total) }
        try expect(progress.totalUnitCount == 7, "Invalid progress widened bounds")
        let transfer = Progress(totalUnitCount: 100)
        configureFileProgress(transfer, operation: .downloading)
        try expect(transfer.kind == .file && transfer.fileOperationKind == .downloading && transfer.isIndeterminate && !transfer.isPausable,
                   "File transfer progress lost its direction or unknown-size state")
        let reporting = SocketOperation(progress: transfer)
        reporting.report(completed: 3, total: nil, bytesPerSecond: 10, etaSeconds: nil)
        try expect(transfer.isIndeterminate && transfer.completedUnitCount == 3 && (transfer.userInfo[.throughputKey] as? NSNumber)?.doubleValue == 10,
                   "Unknown transfer length became an invented percentage")
        reporting.report(completed: 10, total: 10, bytesPerSecond: -.infinity, etaSeconds: -1)
        try expect(transfer.completedUnitCount == 9 && transfer.userInfo[.throughputKey] == nil && transfer.userInfo[.estimatedTimeRemainingKey] == nil,
                   "Unconfirmed bytes reached 100 percent or invalid rate/ETA leaked")
        reporting.report(completed: 5, total: 10, bytesPerSecond: 20, etaSeconds: 0.25)
        try expect((transfer.userInfo[.estimatedTimeRemainingKey] as? NSNumber)?.doubleValue == 0.25, "Valid byte ETA was discarded")
        reporting.finishProgress(); reporting.finishProgress()
        reporting.report(completed: 1, total: 10)
        try expect(transfer.completedUnitCount == 10 && (transfer.userInfo[.fileCompletedCountKey] as? NSNumber)?.intValue == 1,
                   "A late progress frame reversed a confirmed receipt")
        let unknown = Progress(totalUnitCount: -1)
        SocketOperation(progress: unknown).finishProgress()
        try expect(unknown.totalUnitCount == 1 && unknown.completedUnitCount == 1, "Confirmed unknown-size transfer did not finish")
        let cancelledProgress = Progress(totalUnitCount: 10)
        let cancelledReporting = SocketOperation(progress: cancelledProgress)
        cancelledReporting.cancel(); cancelledReporting.report(completed: 5, total: 10); cancelledReporting.finishProgress()
        try expect(cancelledProgress.completedUnitCount == 0, "Cancelled work was reported as confirmed")
        let observed = Progress(totalUnitCount: 10)
        let reentrant = SocketOperation(progress: observed)
        let observation = observed.observe(\.completedUnitCount) { _, _ in reentrant.cancel() }
        reentrant.report(completed: 5, total: 10)
        observation.invalidate(); reentrant.finishProgress()
        try expect(observed.completedUnitCount == 5, "Reentrant KVO cancellation completed the transfer")
        SocketOperation().report(completed: 1, total: 2)
        SocketOperation().finishProgress()
        var snapshot: [String: Any] = ["transfer_id": "attempt", "id": "file", "operation": "upload", "completed": 2,
            "total": 10, "bytes_per_second": 4, "eta_seconds": 2, "state": "transferring", "background": true]
        try expect(try NativeTransferSnapshot.decode(snapshot).total == 10, "Valid transfer progress did not decode")
        snapshot["total"] = NSNull(); snapshot["eta_seconds"] = NSNull()
        try expect(try NativeTransferSnapshot.decode(snapshot).total == nil, "Unknown transfer length was invented")
        for change: [String: Any] in [["transfer_id": ""], ["id": ""], ["operation": "rename"], ["state": "unknown"],
                ["completed": -1], ["completed": NSNumber(value: UInt64.max)], ["total": 1], ["bytes_per_second": -1], ["eta_seconds": -1]] {
            var invalid = snapshot; invalid.merge(change) { _, next in next }
            try rejects("Invalid transfer snapshot accepted") { _ = try NativeTransferSnapshot.decode(invalid) }
        }
        let indeterminate = Progress(totalUnitCount: -1)
        _ = try bridge.request(["op": "test.indeterminate-progress"], operation: SocketOperation(progress: indeterminate))
        try expect(indeterminate.isIndeterminate && indeterminate.completedUnitCount == 3, "Unknown bridge totals did not stay indeterminate")
        let preflight = Progress(totalUnitCount: 100)
        _ = try bridge.request(["op": "test.fragmented"], operation: SocketOperation(progress: preflight), reportsProgress: false)
        try expect(preflight.totalUnitCount == 100 && preflight.completedUnitCount == 0, "Preflight request reset payload progress")
        operation.cancel()
        try rejects("Cancelled operation attached a socket") { try operation.attach(-1) }
        try rejects("Precancelled request started I/O") { _ = try bridge.request(["op": "test.fragmented"], operation: operation) }
        for op in ["test.nonobject", "test.malformed", "test.disconnected", "test.error"] {
            try rejects("Invalid/disconnected backend response accepted") { _ = try bridge.request(["op": op]) }
        }
        do { _ = try bridge.request(["op": "test.oversized"]); throw TestFailure(message: "Oversized bridge response accepted") }
        catch let NativeFailure.invalid(message) { try expect(message.contains("too large"), "Large response failed for an unrelated reason") }
        try expect(try bridge.item(["op": "item"]).id == "stable-0", "Item response did not decode")
        try rejects("Missing backend item accepted") { _ = try bridge.item(["op": "test.missing-item"]) }
        try rejects("Oversized bridge request accepted") { _ = try bridge.request(["op": "unused", "text": String(repeating: "x", count: 4 * 1024 * 1024)]) }
        let long = BackendBridge(paths: NativePaths(group: paths.group.appendingPathComponent(String(repeating: "x", count: 110))))
        try rejects("Oversized Unix socket path accepted") { _ = try long.request(["op": "unused"]) }
        let absent = BackendBridge(paths: NativePaths(group: paths.group.appendingPathComponent("missing-peer")))
        try rejects("Missing Unix peer did not fail") { _ = try absent.request(["op": "unused"]) }
        try rejects("Missing development endpoint accepted") {
            _ = try BackendBridge(paths: NativePaths(group: paths.group.appendingPathComponent("missing-peer"), development: true)).request(["op": "unused"])
        }
        try rejects("Directory was accepted as upload content") { _ = try bridge.stagedCopy(paths.group) }
        print("PASS strict wire/attribute validation, typed errors, cancellation and malformed/disconnected IPC responses")
    }

    static func testConfiguredPaths(_ paths: NativePaths) throws {
        func bundle(_ values: [String: Any]) throws -> Bundle {
            let url = paths.group.appendingPathComponent(UUID().uuidString + ".bundle")
            let contents = url.appendingPathComponent("Contents")
            try FileManager.default.createDirectory(at: contents, withIntermediateDirectories: true)
            let data = try PropertyListSerialization.data(fromPropertyList: values.merging(["CFBundleIdentifier": UUID().uuidString]) { left, _ in left }, format: .xml, options: 0)
            try data.write(to: contents.appendingPathComponent("Info.plist"))
            guard let bundle = Bundle(url: url) else { throw TestFailure(message: "Could not create a synthetic bundle") }
            return bundle
        }
        let valid = try bundle(["RDriveDevelopmentIPCRoot": paths.group.path])
        let selected = try NativePaths.configured(bundle: valid)
        try expect(selected.development && selected.group.standardizedFileURL.path == paths.group.standardizedFileURL.path, "Private development bundle path changed")
        try expect(try BackendBridge(bundle: valid).paths.group.standardizedFileURL.path == paths.group.standardizedFileURL.path, "Bundle bridge did not use selected path")
        let appGroup = try bundle(["RDriveAppGroup": "synthetic.coverage.group"])
        let grouped = try NativePaths.configured(bundle: appGroup, groupContainer: { identifier in
            identifier == "synthetic.coverage.group" ? paths.group : nil
        })
        try expect(grouped.group == paths.group && !grouped.development, "Authorized-container boundary changed its configured result")
        try rejects("Missing app group accepted") { _ = try NativePaths.configured(bundle: bundle([:])) }
        try rejects("Relative development path accepted") { _ = try NativePaths.configured(bundle: bundle(["RDriveDevelopmentIPCRoot": "relative"])) }
        let publicDirectory = paths.group.appendingPathComponent("public-test")
        try? FileManager.default.removeItem(at: publicDirectory)
        try FileManager.default.createDirectory(at: publicDirectory, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o755])
        try rejects("Public development directory accepted") { _ = try NativePaths.configured(bundle: bundle(["RDriveDevelopmentIPCRoot": publicDirectory.path])) }
        let alias = paths.group.appendingPathComponent("group-alias")
        try? FileManager.default.removeItem(at: alias)
        try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: paths.group)
        try rejects("Symlink development directory accepted") { _ = try NativePaths.configured(bundle: bundle(["RDriveDevelopmentIPCRoot": alias.path])) }
        print("PASS synthetic bundle configuration, private path selection and unsafe/missing path refusal")
    }

    static func testProviderProperties() throws {
        var properties: [String: Any] = ["capabilities": ["read", "write", "add", "rename", "reparent", "delete", "unknown"], "modified": "2026-11-10T10:00:00Z"]
        let regular = ProviderItem(try fixtureItem(properties))
        try expect(regular.filename == "test.txt" && regular.documentSize == 19 && regular.contentType == .plainText, "Regular file properties changed")
        try expect(regular.contentModificationDate != nil && regular.contentPolicy == .inherited, "Unpinned file/date properties changed")
        try expect(regular.capabilities.contains(.allowsReading) && regular.capabilities.contains(.allowsReparenting) && regular.capabilities.contains(.allowsDeleting), "Native capability projection incomplete")
        try expect(regular.itemVersion.contentVersion == Data("content".utf8), "Content version changed")
        try expect(regular.tagData == nil && regular.extendedAttributes.isEmpty && regular.uploadingError == nil && regular.isUploaded && !regular.isUploading, "Clean defaults changed")
        properties["modified"] = "not-a-date"; properties["name"] = "unknown-no-extension"
        try expect(ProviderItem(fixtureItem(properties)).contentModificationDate == nil && ProviderItem(fixtureItem(properties)).contentType == .data, "Unknown content/date fallback changed")
        let directory = ProviderItem(try fixtureItem(["kind": "directory", "id": "root"]))
        try expect(directory.documentSize == nil && directory.contentType == .folder && directory.contentPolicy == .downloadLazily && directory.decorations == nil && directory.contentModificationDate == nil, "Root directory properties changed")
        let refreshRule = NSPredicate(format: "fileproviderItems.@count == 1 AND " +
            "SUBQUERY(fileproviderItems, $item, $item.userInfo.rdriveSmart == YES AND $item.contentType.identifier == 'public.folder').@count == 1")
        let smartDirectory = ProviderItem(try fixtureItem(["kind": "directory", "smart": true]))
        let smartFile = ProviderItem(try fixtureItem(["smart": true]))
        try expect(smartDirectory.userInfo?["rdriveDirectory"] == nil, "Legacy item fixture unexpectedly includes a directory marker")
        for (items, visible) in [([smartDirectory], true), ([smartFile], false), ([directory], false), ([], false), ([smartDirectory, smartDirectory], false)] {
            try expect(refreshRule.evaluate(with: ["fileproviderItems": items]) == visible,
                       "Refresh activation rejected a cached Smart directory or accepted an invalid selection")
        }
        for state in ["syncing", "conflict", "error"] {
            let item = ProviderItem(try fixtureItem(["sync_state": state]))
            try expect(item.isUploading == (state == "syncing"), "Uploading state projection changed")
            try expect((item.uploadingError != nil) == (state != "syncing"), "Error state projection changed")
        }
        let dirty = ProviderItem(try fixtureItem(["has_unsynced_changes": true]))
        try expect(!dirty.isUploaded, "Explicit unsynced flag was ignored")
        let blocked = ProviderItem(try fixtureItem(["sync_state": "blocked", "sync_message": "Excluded by synchronization filters: backup.part"]))
        try expect(!blocked.isUploaded && !blocked.isUploading && (blocked.uploadingError as NSError?)?.code == NSFileProviderError.cannotSynchronize.rawValue,
                   "A filter-blocked local edit was reported as uploaded or unreachable")
        try expect(blocked.uploadingError?.localizedDescription.contains("backup.part") == true && NativeSyncState.blocked.title == "Excluded by sync rules",
                   "The filter review reason was hidden")
        try expect(ProviderItem(try fixtureItem(["sync_state": "blocked"])).uploadingError?.localizedDescription.contains("filters") == true,
                   "A blocked item lost its default review reason")
        let pinnedDirectory = ProviderItem(try fixtureItem(["kind": "directory", "pinned": true]))
        let pinnedFile = ProviderItem(try fixtureItem(["pinned": true]))
        try expect(pinnedDirectory.contentPolicy == .downloadEagerlyAndKeepDownloaded && pinnedFile.contentPolicy == .downloadEagerlyAndKeepDownloaded, "Pinned content policy changed")
        #if RDRIVE_HAS_NAMESPACE_POLICY
        if #available(macOS 27.0, *) {
            try expect(directory.namespacePolicy == .materializeLazily, "Root namespace policy changed")
            try expect(pinnedDirectory.namespacePolicy == .materializeEagerly, "Pinned directory namespace policy changed")
            try expect(regular.namespacePolicy == .inherited, "File namespace policy changed")
        }
        #endif
        print("PASS complete file/directory properties, version/capability projection, native policies and error states")
    }

    static func testBudgetLocalState(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        let budget = NativeMaterializationBudget(bridge: bridge, domain: "local-fixture")
        let state = paths.group.appendingPathComponent("native-budget.json")
        let lock = paths.group.appendingPathComponent("native-budget.lock")
        try? FileManager.default.removeItem(at: state); try? FileManager.default.removeItem(at: lock)
        budget.release(nil); try budget.renew(nil); try budget.markDelivered(nil)
        try expect(try budget.reserve(fixtureItem(), operation: nil) == nil, "Non-Smart content unexpectedly requested system quota")
        try budget.sweep() // The synthetic peer reports no Smart domains: no OS manager is queried.
        try rejects("Oversized Smart content was admitted before its size check") {
            _ = try budget.reserve(fixtureItem(["smart": true, "size": 1025]), operation: nil)
        }
        try budget.acknowledgeLocalContent(fixtureItem())
        try budget.acknowledgeLocalContent(fixtureItem(["smart": true, "kind": "directory"]))
        try budget.acknowledgeLocalContent(fixtureItem(["smart": true, "has_unsynced_changes": true]))
        try expect(!FileManager.default.fileExists(atPath: state.path), "Unconfirmed/nonfile content created a receipt")
        try budget.acknowledgeLocalContent(fixtureItem(["smart": true]))
        func readState() throws -> [String: Any] { try JSONSerialization.jsonObject(with: Data(contentsOf: state)) as! [String: Any] }
        var reservations = try readState()["reservations"] as! [String: [String: Any]]
        try expect(reservations.count == 1, "Confirmed local content did not create one receipt")
        let token = reservations.keys.first!
        try expect(reservations[token]?["delivered"] as? Bool == true && (reservations[token]?["bytes"] as? NSNumber)?.intValue == 0, "Local content incorrectly allocated download bytes")
        try budget.renew([token, "missing"])
        try budget.markDelivered([token, "missing"])
        reservations = try readState()["reservations"] as! [String: [String: Any]]
        try expect(reservations[token]?["pinPreflight"] as? Bool == false, "Delivered reservation retained pin ownership")
        budget.release([token, "missing"])
        try expect((try readState()["reservations"] as! [String: Any]).isEmpty, "Released receipt persisted")
        try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: state.path)
        try rejects("Public reservation state accepted") { try budget.renew([]) }
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: state.path)
        try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: lock.path)
        try rejects("Public budget lock accepted") { try budget.renew([]) }
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: lock.path)
        try FileManager.default.removeItem(at: state)
        try FileManager.default.createSymbolicLink(at: state, withDestinationURL: paths.group.appendingPathComponent("unrelated"))
        try rejects("Symlink reservation file accepted") { try budget.markDelivered([]) }
        try FileManager.default.removeItem(at: state)
        try FileManager.default.removeItem(at: lock)
        try FileManager.default.createSymbolicLink(at: lock, withDestinationURL: paths.group.appendingPathComponent("unrelated"))
        try rejects("Symlink lock accepted") { try budget.renew([]) }
        try FileManager.default.removeItem(at: lock)
        print("PASS local native budget receipts/leases, zero-byte acknowledgements and private-file guards without system cache operations")
    }

    static func testEnumeratorFailures(_ bridge: BackendBridge) throws {
        let enumerator = ProviderEnumerator(domain: "edge-fixture", container: .workingSet, bridge: bridge)
        let anchored = DispatchSemaphore(value: 0)
        var anchor: NSFileProviderSyncAnchor?
        enumerator.currentSyncAnchor { anchor = $0; anchored.signal() }
        try wait(anchored)
        try expect(anchor?.rawValue == Data("999".utf8), "Fresh current anchor did not come from the backend")
        for data in [Data([255]), Data("unknown:0".utf8)] {
            let observer = EnumerationProbe()
            enumerator.enumerateItems(for: observer, startingAt: NSFileProviderPage(data))
            try wait(observer.done)
            try expect(observer.error != nil && observer.items.isEmpty, "Invalid page was accepted")
        }
        let first = EnumerationProbe()
        enumerator.enumerateItems(for: first, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByDate as Data))
        try wait(first.done)
        let snapshot = String(data: first.next!.rawValue, encoding: .utf8)!.split(separator: ":")[0]
        for suffix in ["-1", "999"] {
            let observer = EnumerationProbe()
            enumerator.enumerateItems(for: observer, startingAt: NSFileProviderPage(Data((snapshot + ":" + suffix).utf8)))
            try wait(observer.done)
            try expect(observer.error != nil && observer.items.isEmpty, "Invalid page offset was accepted")
        }
        let changes = ChangesProbe()
        enumerator.enumerateChanges(for: changes, from: NSFileProviderSyncAnchor(Data("1".utf8)))
        try wait(changes.done)
        try expect(changes.error == nil && changes.items.count == 3, "Working-set changes were incorrectly parent filtered")
        let invalid = ChangesProbe()
        enumerator.enumerateChanges(for: invalid, from: NSFileProviderSyncAnchor(Data([255])))
        try wait(invalid.done)
        try expect(invalid.error != nil, "Invalid UTF-8 sync anchor accepted")
        enumerator.invalidate()
        let cancelled = ChangesProbe()
        enumerator.enumerateChanges(for: cancelled, from: NSFileProviderSyncAnchor(Data("1".utf8)))
        try wait(cancelled.done)
        try expect(cancelled.error != nil, "Invalidated changes enumerator emitted a success")
        let finished = DispatchSemaphore(value: 0)
        enumerator.currentSyncAnchor { anchor = $0; finished.signal() }
        try wait(finished)
        try expect(anchor == nil, "Invalidated enumerator produced an anchor")
        for domain in ["missing-list", "missing-list-anchor", "invalid-list-item"] {
            let invalid = ProviderEnumerator(domain: domain, container: .rootContainer, bridge: bridge)
            let observer = EnumerationProbe()
            invalid.enumerateItems(for: observer, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
            try wait(observer.done)
            try expect(observer.error != nil && observer.items.isEmpty, "Incomplete listing was treated as an empty success")
            invalid.invalidate()
        }
        for domain in ["missing-changes", "missing-changes-anchor"] {
            let invalid = ProviderEnumerator(domain: domain, container: .rootContainer, bridge: bridge)
            let observer = ChangesProbe()
            invalid.enumerateChanges(for: observer, from: NSFileProviderSyncAnchor(Data("1".utf8)))
            try wait(observer.done)
            try expect(observer.error != nil, "Incomplete changes were acknowledged")
            invalid.invalidate()
        }
        let missing = ProviderEnumerator(domain: "missing-changes-anchor", container: .rootContainer, bridge: bridge)
        let missingDone = DispatchSemaphore(value: 0)
        missing.currentSyncAnchor { anchor = $0; missingDone.signal() }
        try wait(missingDone)
        try expect(anchor == nil, "Missing current anchor was manufactured")
        missing.invalidate()
        print("PASS invalid/expired pagination, current/invalidated anchors, working-set projection and incomplete catalog refusal")
    }

    static func main() {
        setbuf(stdout, nil)
        do { try run() }
        catch {
            fflush(stdout)
            fputs("FAIL native tests: \(error)\n", stderr)
            exit(1)
        }
    }

    static func run() throws {
        guard (2...3).contains(CommandLine.arguments.count) else { throw TestFailure(message: "Pass an isolated bridge directory") }
        let development = CommandLine.arguments.last == "--development-tcp"
        let paths = NativePaths(group: URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true), development: development)
        let bridge = BackendBridge(paths: paths)
        try testFinderTagNormalization()
        try testValidation(paths, bridge)
        try testConfiguredPaths(paths)
        try testProviderProperties()
        try testBudgetLocalState(paths, bridge)
        try testEnumeratorFailures(bridge)
        try ProviderCoverageTests.run(paths, bridge)
        try BudgetCoverageTests.run(paths, bridge)
        try UICoverageTests.run(paths, bridge)
        try CompanionCoverageTests.run(paths, bridge)
        var raw: [String: Any] = ["id": "stable-uuid", "parent_id": "root", "name": "notes.txt", "kind": "file", "size": 19,
            "modified": "2026-11-09T12:00:00.123Z", "content_version": "content-A", "metadata_version": "meta-A", "pinned": true,
            "capabilities": ["read", "write", "rename"], "tag_data_hex": "0102ff",
            "extended_attributes": ["user.rdrive.test#S": "0001", "com.apple.quarantine": "02"]]
        let item = ProviderItem(try WireItem.decode(raw))
        try expect(item.itemIdentifier.rawValue == "stable-uuid", "Provider identity changed")
        try expect(item.parentItemIdentifier == .rootContainer, "Root mapping failed")
        try expect(item.contentPolicy == .downloadEagerlyAndKeepDownloaded, "Pin did not set native content policy")
        try expect(item.capabilities.contains(.allowsWriting) && !item.capabilities.contains(.allowsDeleting), "Capabilities widened")
        try expect(item.tagData == Data([1, 2, 255]), "Finder tags changed")
        try expect(item.extendedAttributes == ["user.rdrive.test#S": Data([0, 1])], "Safe native attributes were lost or protected attributes leaked")
        try expect(item.contentModificationDate != nil, "Fractional ISO date failed")
        raw["name"] = "../escape"
        do { _ = try WireItem.decode(raw); throw TestFailure(message: "Unsafe name accepted") } catch is NativeFailure { }
        raw["name"] = "notes.txt"; raw["content_version"] = String(repeating: "x", count: 129)
        do { _ = try WireItem.decode(raw); throw TestFailure(message: "Oversized version accepted") } catch is NativeFailure { }
        try expect(remoteError(["code": "no_such_item"]).code == NSFileProviderError.noSuchItem.rawValue, "Incorrect provider error")
        print("PASS native item versions, policies, capabilities, tags, timestamps and validation")
        raw["content_version"] = "content-A"; raw["name"] = "notes.txt"
        for state in NativeSyncState.allCases {
            raw["sync_state"] = state.rawValue; raw["smart"] = true; raw["sync_message"] = "Synthetic status"
            let decorated = ProviderItem(try WireItem.decode(raw))
            try expect(decorated.decorations?.first?.rawValue == "io.rdrive.native.provider.status." + state.rawValue, "Native status decoration missing")
            try expect(decorated.decorations?.count == (state.badgeType == nil ? 1 : 2), "Native status badge mismatch")
            try expect(decorated.userInfo?["rdriveSmart"] as? Bool == true, "Smart mode metadata missing")
            if state == .queued || state == .conflict { try expect(!decorated.isUploaded, "Pending content incorrectly marked uploaded/evictable") }
        }
        print("PASS SDK File Provider status decorations, Smart mode and protected pending content")

        let progress = Progress(totalUnitCount: 100)
        let operation = SocketOperation(progress: progress)
        let response = try bridge.request(["op": "test.fragmented"], operation: operation)
        try expect(response["value"] as? String == "fragmented reply", "Fragmented response was lost")
        try expect(progress.totalUnitCount == 19 && progress.completedUnitCount == 18, "Transport bytes prematurely confirmed the operation")
        operation.finishProgress()
        try expect(progress.completedUnitCount == 19, "Confirmed byte progress was not completed")
        do { _ = try bridge.request(["op": "test.authentication"]); throw TestFailure(message: "Backend error was ignored") }
        catch let error as NSError { try expect(error.domain == NSFileProviderErrorDomain && error.code == NSFileProviderError.notAuthenticated.rawValue, "Wrong auth error mapping") }
        let cancel = SocketOperation()
        let finished = DispatchSemaphore(value: 0)
        var cancellation: NSError?
        DispatchQueue.global().async {
            do { _ = try bridge.request(["op": "test.cancel"], operation: cancel) }
            catch { cancellation = error as NSError }
            finished.signal()
        }
        Thread.sleep(forTimeInterval: 0.1); cancel.cancel(); try wait(finished)
        try expect(cancellation?.domain == NSCocoaErrorDomain && cancellation?.code == NSUserCancelledError, "Cancellation did not interrupt the socket")
        print("PASS actual Unix socket framing, progress, typed errors and cancellation")

        let enumerator = ProviderEnumerator(domain: "fixture", container: .rootContainer, bridge: bridge)
        let first = EnumerationProbe()
        enumerator.enumerateItems(for: first, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        try wait(first.done); try expect(first.error == nil && first.items.count == 1 && first.next != nil, "First page failed")
        let second = EnumerationProbe()
        enumerator.enumerateItems(for: second, startingAt: first.next!)
        try wait(second.done); try expect(second.error == nil && second.items.count == 1 && second.next != nil, "Second page failed")
        let third = EnumerationProbe()
        enumerator.enumerateItems(for: third, startingAt: second.next!)
        try wait(third.done); try expect(third.error == nil && third.items.count == 1 && third.next == nil, "Final page failed")
        try expect(Set((first.items + second.items + third.items).map { $0.itemIdentifier.rawValue }).count == 3, "Page overlap or missing identity")
        let anchored = DispatchSemaphore(value: 0)
        var snapshotAnchor: NSFileProviderSyncAnchor?
        enumerator.currentSyncAnchor { snapshotAnchor = $0; anchored.signal() }
        try wait(anchored)
        try expect(snapshotAnchor?.rawValue == Data("2".utf8), "Enumeration advanced beyond the delivered snapshot and could skip changes")
        let changes = ChangesProbe()
        enumerator.enumerateChanges(for: changes, from: NSFileProviderSyncAnchor(Data("1".utf8)))
        try wait(changes.done)
        try expect(changes.error == nil && changes.items.count == 3 && changes.deleted.count == 1 && changes.anchor?.rawValue == Data("2".utf8), "Journal change delivery failed")
        let scoped = ProviderEnumerator(domain: "fixture", container: providerID("folder-one"), bridge: bridge)
        let nested = ChangesProbe()
        scoped.enumerateChanges(for: nested, from: NSFileProviderSyncAnchor(Data("1".utf8)))
        try wait(nested.done)
        try expect(nested.error == nil && nested.items.count == 1 && nested.items[0].parentItemIdentifier == providerID("folder-one"), "Directory changes leaked items from another parent")
        scoped.invalidate()
        enumerator.invalidate()
        let invalid = EnumerationProbe()
        enumerator.enumerateItems(for: invalid, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        try wait(invalid.done); try expect(invalid.error != nil && invalid.items.isEmpty, "Invalidated enumerator emitted items")
        print("PASS SDK enumeration observers, snapshot paging, journal updates/deletions and invalidation")

        let source = paths.group.appendingPathComponent("source.bin")
        try Data("actual content".utf8).write(to: source)
        let staged = try bridge.stagedCopy(source)
        let stagedBytes = try Data(contentsOf: staged)
        try expect(stagedBytes == Data("actual content".utf8), "Upload staging changed content")
        let alias = paths.group.appendingPathComponent("alias")
        try? FileManager.default.removeItem(at: alias)
        try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: source)
        do { _ = try bridge.stagedCopy(alias); throw TestFailure(message: "Symlink content accepted") } catch is NativeFailure { }
        try FileManager.default.removeItem(at: staged)
        print("PASS private regular-file staging and symlink refusal")
        if development {
            let endpoint = paths.group.appendingPathComponent("bridge-endpoint.json")
            let original = try Data(contentsOf: endpoint)
            var descriptor = try JSONSerialization.jsonObject(with: original) as! [String: Any]
            descriptor["host"] = "192.0.2.1"
            try JSONSerialization.data(withJSONObject: descriptor).write(to: endpoint)
            do { _ = try bridge.request(["op": "test.fragmented"]); throw TestFailure(message: "Non-loopback IPC destination accepted") } catch is NativeFailure { }
            try original.write(to: endpoint)
            try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: endpoint.path)
            do { _ = try bridge.request(["op": "test.fragmented"]); throw TestFailure(message: "Public IPC nonce descriptor accepted") } catch is NativeFailure { }
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: endpoint.path)
            let backup = paths.group.appendingPathComponent("endpoint-private-backup")
            try FileManager.default.moveItem(at: endpoint, to: backup)
            try FileManager.default.createSymbolicLink(at: endpoint, withDestinationURL: backup)
            do { _ = try bridge.request(["op": "test.fragmented"]); throw TestFailure(message: "Symlink IPC descriptor accepted") } catch is NativeFailure { }
            try FileManager.default.removeItem(at: endpoint)
            try FileManager.default.moveItem(at: backup, to: endpoint)
            print("PASS development TCP requires localhost, private nonce descriptor and no symlink")
        }
        try NativeSDKCoverageTests.run(paths)
    }
}
