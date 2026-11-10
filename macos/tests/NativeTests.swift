import Foundation
import FileProvider
import UniformTypeIdentifiers

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
        print("PASS native tag archive normalization, exact raw bytes, explicit clear, names/colors and size bounds")
    }

    static func main() throws {
        guard (2...3).contains(CommandLine.arguments.count) else { throw TestFailure(message: "Pass an isolated bridge directory") }
        let development = CommandLine.arguments.last == "--development-tcp"
        let paths = NativePaths(group: URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true), development: development)
        let bridge = BackendBridge(paths: paths)
        try testFinderTagNormalization()
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
        try expect(progress.totalUnitCount == 19 && progress.completedUnitCount == 19, "Streaming byte progress was lost")
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
    }
}
