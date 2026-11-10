import Foundation
import FileProvider

final class ProviderEnumerator: NSObject, NSFileProviderEnumerator {
    let domain: String
    let container: NSFileProviderItemIdentifier
    let bridge: BackendBridge
    private let lock = NSLock()
    private var valid = true
    private let operation = SocketOperation()
    private var snapshot: [ProviderItem] = []
    private var snapshotID = ""
    private var snapshotAnchor: Data?
    private let queue = DispatchQueue(label: "io.rdrive.provider.enumerator", qos: .utility)
    init(domain: String, container: NSFileProviderItemIdentifier, bridge: BackendBridge) {
        self.domain = domain; self.container = container; self.bridge = bridge
    }
    func invalidate() { lock.lock(); valid = false; snapshot = []; lock.unlock(); operation.cancel() }
    private func check() throws {
        lock.lock(); let active = valid; lock.unlock()
        if !active { throw NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError) }
    }
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        queue.async {
            do {
                try self.check()
                let start: Int
                if page.rawValue == NSFileProviderPage.initialPageSortedByName as Data || page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data {
                    let result = try self.bridge.request(["op": "list", "domain": self.domain, "parent_id": wireID(self.container)], operation: self.operation)
                    guard let rows = result["items"] as? [Any] else { throw NativeFailure.invalid("Backend omitted the directory listing.") }
                    let items = try rows.map { ProviderItem(try WireItem.decode($0)) }
                    guard let anchor = result["anchor"] as? String else { throw NativeFailure.invalid("Backend omitted the listing anchor.") }
                    self.lock.lock(); self.snapshot = items; self.snapshotID = UUID().uuidString; self.snapshotAnchor = Data(anchor.utf8); self.lock.unlock()
                    start = 0
                } else {
                    guard let token = String(data: page.rawValue, encoding: .utf8) else { throw remoteError(["code": "sync_anchor_expired"]) }
                    let parts = token.split(separator: ":")
                    self.lock.lock(); let current = self.snapshotID; self.lock.unlock()
                    guard parts.count == 2, String(parts[0]) == current, let offset = Int(parts[1]), offset >= 0 else { throw remoteError(["code": "sync_anchor_expired"]) }
                    start = offset
                }
                try self.check()
                self.lock.lock(); let items = self.snapshot; let token = self.snapshotID; self.lock.unlock()
                guard start <= items.count else { throw remoteError(["code": "sync_anchor_expired"]) }
                let pageSize = max(1, min(observer.suggestedPageSize ?? 200, 1000))
                let end = min(start + pageSize, items.count)
                observer.didEnumerate(Array(items[start..<end]))
                observer.finishEnumerating(upTo: end < items.count ? NSFileProviderPage(Data("\(token):\(end)".utf8)) : nil)
            } catch { nativeDevelopmentLog("Provider enumeration: \(error)"); observer.finishEnumeratingWithError(providerError(error)) }
        }
    }
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        queue.async {
            do {
                try self.check()
                guard let token = String(data: anchor.rawValue, encoding: .utf8) else { throw remoteError(["code": "sync_anchor_expired"]) }
                let result = try self.bridge.request(["op": "changes", "domain": self.domain, "parent_id": wireID(self.container), "anchor": token], operation: self.operation)
                guard let rows = result["items"] as? [Any], let deletedIDs = result["deleted"] as? [String] else { throw NativeFailure.invalid("Backend omitted the change list.") }
                let allItems = try rows.map { ProviderItem(try WireItem.decode($0)) }
                let items = self.container == .workingSet ? allItems : allItems.filter { $0.parentItemIdentifier == self.container }
                let deleted = deletedIDs.map(providerID)
                guard let next = result["anchor"] as? String else { throw NativeFailure.invalid("Backend omitted the sync anchor.") }
                try self.check()
                let batchSize = max(1, min(observer.suggestedBatchSize ?? 200, 1000))
                for start in stride(from: 0, to: items.count, by: batchSize) { observer.didUpdate(Array(items[start..<min(start + batchSize, items.count)])) }
                for start in stride(from: 0, to: deleted.count, by: batchSize) { observer.didDeleteItems(withIdentifiers: Array(deleted[start..<min(start + batchSize, deleted.count)])) }
                self.lock.lock(); self.snapshotAnchor = Data(next.utf8); self.lock.unlock()
                observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(Data(next.utf8)), moreComing: result["more"] as? Bool ?? false)
            } catch { nativeDevelopmentLog("Provider changes: \(error)"); observer.finishEnumeratingWithError(providerError(error)) }
        }
    }
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        queue.async {
            do {
                try self.check()
                self.lock.lock(); let snapshotAnchor = self.snapshotAnchor; self.lock.unlock()
                if let snapshotAnchor = snapshotAnchor { completionHandler(NSFileProviderSyncAnchor(snapshotAnchor)); return }
                let result = try self.bridge.request(["op": "changes", "domain": self.domain, "parent_id": wireID(self.container), "anchor": "current"], operation: self.operation)
                guard let anchor = result["anchor"] as? String else { throw NativeFailure.invalid("Backend omitted the sync anchor.") }
                completionHandler(NSFileProviderSyncAnchor(Data(anchor.utf8)))
            } catch { completionHandler(nil) }
        }
    }
}
