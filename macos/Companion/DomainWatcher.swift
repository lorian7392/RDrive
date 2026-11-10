import Foundation
import FileProvider

/// Keeps native domains current even when macOS unloads an idle extension.
/// It reads the local Rust catalog; remote synchronization stays in the broker.
final class NativeDomainWatcher {
    private let bridge: BackendBridge
    private let queue = DispatchQueue(label: "io.rdrive.native.domain-watcher", qos: .utility)
    private let lock = NSLock()
    private let operation = SocketOperation()
    private var busy = false
    private var stopped = false
    private var anchors: [String: String] = [:]
    private var parents: [String: [String: String]] = [:]
    private var pendingPins: [String: [String: WireItem]] = [:]
    private var queuedParents: [String: Set<String>] = [:]
    private var lastRequests: [String: Date] = [:]
    private var lastSweep = Date.distantPast

    private let system: NativeSystemManaging
    private let timeout: TimeInterval
    private let now: () -> Date
    init(bridge: BackendBridge, system: NativeSystemManaging = NativeSystemManager(),
         timeout: TimeInterval = 20, now: @escaping () -> Date = Date.init) {
        self.bridge = bridge; self.system = system; self.timeout = timeout; self.now = now
    }
    func stop() { lock.lock(); stopped = true; lock.unlock(); operation.cancel() }
    func poll(completion: ((Error?) -> Void)? = nil) {
        lock.lock()
        guard !busy && !stopped else { lock.unlock(); completion?(nil); return }
        busy = true; lock.unlock()
        queue.async {
            var failure: Error?
            defer { self.lock.lock(); self.busy = false; self.lock.unlock(); completion?(failure) }
            do { try self.tick() }
            catch { nativeDevelopmentLog("Native domain watcher: \(error)"); failure = error }
        }
    }
    private func domains() throws -> [NSFileProviderDomain] {
        let done = DispatchSemaphore(value: 0)
        var result: [NSFileProviderDomain] = []; var failure: Error?
        system.domains { result = $0; failure = $1; done.signal() }
        guard done.wait(timeout: .now() + timeout) == .success else { throw NativeFailure.bridge("The native domain list timed out.") }
        if let failure = failure { throw failure }
        return result
    }
    private func tick() throws {
        try operation.check()
        let configured = try bridge.request(["op": "domains"], operation: operation)["domains"] as? [[String: Any]] ?? []
        let smart = Set(configured.filter { $0["smart"] as? Bool == true }.compactMap { $0["id"] as? String })
        for domain in try domains() where smart.contains(domain.identifier.rawValue) {
            try operation.check()
            let id = domain.identifier.rawValue
            guard let manager = system.manager(for: domain) else { throw NativeFailure.bridge("A Smart Finder domain manager is unavailable.") }
            let reply: [String: Any]
            if let anchor = anchors[id] {
                do { reply = try bridge.request(["op": "changes", "domain": id, "anchor": anchor, "parent_id": "working_set"], operation: operation) }
                catch let error as NSError where error.domain == NSFileProviderErrorDomain && error.code == NSFileProviderError.syncAnchorExpired.rawValue {
                    anchors.removeValue(forKey: id)
                    queuedParents[id, default: []].formUnion((parents[id] ?? [:]).values)
                    parents.removeValue(forKey: id); pendingPins.removeValue(forKey: id)
                    continue
                }
            } else { reply = try bridge.request(["op": "list", "domain": id, "parent_id": "working_set"], operation: operation) }
            guard let rows = reply["items"] as? [Any], let anchor = reply["anchor"] as? String else {
                throw NativeFailure.invalid("The Smart catalog omitted its items or anchor.")
            }
            let items = try rows.map(WireItem.decode)
            var changedParents = Set<String>()
            if anchors[id] == nil {
                changedParents.formUnion((parents[id] ?? [:]).values)
                parents[id] = [:]; pendingPins[id] = [:]
            }
            for item in items {
                if let previous = parents[id]?[item.id] { changedParents.insert(previous) }
                changedParents.insert(item.parent_id)
                parents[id, default: [:]][item.id] = item.parent_id
                if item.kind == "file" && item.pinned && item.has_unsynced_changes != true && item.native_materialized != true {
                    pendingPins[id, default: [:]][item.id] = item
                } else { pendingPins[id]?.removeValue(forKey: item.id) }
            }
            for deleted in reply["deleted"] as? [String] ?? [] {
                if let previous = parents[id]?.removeValue(forKey: deleted) { changedParents.insert(previous) }
                pendingPins[id]?.removeValue(forKey: deleted)
            }
            anchors[id] = anchor
            for item in (pendingPins[id] ?? [:]).values { changedParents.insert(item.parent_id) }
            queuedParents[id, default: []].formUnion(changedParents)
            if !(queuedParents[id] ?? []).isEmpty {
                manager.signalEnumerator(for: .workingSet) { _ in }
                for parent in (queuedParents[id] ?? []).sorted().prefix(256) {
                    queuedParents[id]?.remove(parent)
                    manager.signalEnumerator(for: providerID(parent)) { error in
                        if error != nil { self.queue.async { self.queuedParents[id, default: []].insert(parent) } }
                    }
                }
            }
            var scheduled = 0
            for item in (pendingPins[id] ?? [:]).values.sorted(by: { $0.id < $1.id }) {
                let key = id + "\n" + item.id
                guard now().timeIntervalSince(lastRequests[key] ?? .distantPast) >= 15 else { continue }
                guard scheduled < 4 else { break }
                scheduled += 1; lastRequests[key] = now()
                // This only schedules the OS request. The authoritative
                // fetchContents callback owns admission and its unique lease;
                // scheduling must never duplicate an in-flight reservation.
                manager.requestDownloadForItem(withIdentifier: providerID(item.id)) { _ in }
            }
        }
        if !smart.isEmpty && now().timeIntervalSince(lastSweep) >= 15 {
            try NativeMaterializationBudget(bridge: bridge, domain: "", system: system, timeout: timeout, now: now).sweep(operation: operation)
            lastSweep = now()
        }
    }
}
