import Foundation
import FileProvider

/// Publishes only worker-owned transfers. Foreground callbacks already return
/// their own Progress to File Provider and must not create a second indicator.
final class NativeBackgroundTransferWatcher {
    private struct Published {
        let item: String
        let direction: String
        let progress: Progress
        let operation: SocketOperation
    }
    private let bridge: BackendBridge
    private let system: NativeSystemManaging
    private let timeout: TimeInterval
    private let publish: (Progress) -> Void
    private let unpublish: (Progress) -> Void
    private let queue = DispatchQueue(label: "io.rdrive.native.transfer-progress", qos: .utility)
    private let lock = NSLock()
    private let operation = SocketOperation()
    private var busy = false
    private var stopped = false
    private var published: [String: Published] = [:]

    init(bridge: BackendBridge, system: NativeSystemManaging = NativeSystemManager(), timeout: TimeInterval = 5,
         publish: @escaping (Progress) -> Void = { $0.publish() },
         unpublish: @escaping (Progress) -> Void = { $0.unpublish() }) {
        self.bridge = bridge; self.system = system; self.timeout = timeout
        self.publish = publish; self.unpublish = unpublish
    }
    func stop(completion: (() -> Void)? = nil) {
        lock.lock(); stopped = true; lock.unlock(); operation.cancel()
        queue.async { self.clear(); completion?() }
    }
    func poll(completion: ((Error?) -> Void)? = nil) {
        lock.lock()
        guard !busy && !stopped else { lock.unlock(); completion?(nil); return }
        busy = true; lock.unlock()
        queue.async {
            var failure: Error?
            defer { self.lock.lock(); self.busy = false; self.lock.unlock(); completion?(failure) }
            do { try self.tick() }
            catch { self.clear(); failure = error; nativeDevelopmentLog("Transfer progress: \(error)") }
        }
    }
    private func clear() {
        for entry in published.values { unpublish(entry.progress) }
        published.removeAll()
    }
    private func visibleURL(_ manager: NativeDomainManaging, id: String) throws -> URL {
        let done = DispatchSemaphore(value: 0)
        var resolved: URL?; var failure: Error?
        manager.getUserVisibleURL(for: providerID(id)) { resolved = $0; failure = $1; done.signal() }
        guard done.wait(timeout: .now() + timeout) == .success else {
            throw NativeFailure.bridge("Resolving the transfer's Finder item timed out.")
        }
        try operation.check()
        if let failure = failure { throw failure }
        guard let resolved = resolved, resolved.isFileURL else {
            throw NativeFailure.invalid("The native manager omitted the transfer's file URL.")
        }
        return resolved
    }
    private func registeredDomains() throws -> Set<String> {
        let done = DispatchSemaphore(value: 0)
        var domains: [NSFileProviderDomain] = []; var failure: Error?
        system.domains { domains = $0; failure = $1; done.signal() }
        guard done.wait(timeout: .now() + timeout) == .success else {
            throw NativeFailure.bridge("The native transfer domain list timed out.")
        }
        try operation.check()
        if let failure = failure { throw failure }
        return Set(domains.map { $0.identifier.rawValue })
    }
    private func tick() throws {
        let configured = try bridge.request(["op": "domains"], operation: operation)["domains"] as? [[String: Any]] ?? []
        let registered = try registeredDomains()
        var present = Set<String>()
        for raw in configured where raw["smart"] as? Bool == true {
            try operation.check()
            guard let domainID = raw["id"] as? String else { throw NativeFailure.invalid("A transfer domain has no identifier.") }
            guard registered.contains(domainID) else { continue }
            let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(domainID), displayName: raw["display_name"] as? String ?? domainID)
            guard let manager = system.manager(for: domain) else { continue }
            let reply = try bridge.request(["op": "transfer_status", "domain": domainID], operation: operation)
            guard let rows = reply["transfers"] as? [Any], rows.count <= 256 else { throw NativeFailure.invalid("The backend omitted its bounded transfer list.") }
            for row in try rows.map(NativeTransferSnapshot.decode) where row.background {
                let key = domainID + "\n" + row.transfer_id
                if ["completed", "failed", "cancelled"].contains(row.state) {
                    if let existing = published.removeValue(forKey: key) {
                        if row.state == "completed" { existing.operation.finishProgress() }
                        unpublish(existing.progress)
                    }
                    continue
                }
                present.insert(key)
                guard present.count <= 256 else { throw NativeFailure.invalid("Too many active transfer progress entries.") }
                if let existing = published[key], existing.item != row.id || existing.direction != row.operation {
                    unpublish(existing.progress); published.removeValue(forKey: key)
                }
                if published[key] == nil {
                    let url: URL
                    do { url = try visibleURL(manager, id: row.id) }
                    catch {
                        try operation.check()
                        present.remove(key)
                        nativeDevelopmentLog("Transfer item unavailable: \(error)")
                        continue
                    }
                    let progress = Progress(totalUnitCount: -1)
                    configureFileProgress(progress, operation: row.operation == "upload" ? .uploading : .downloading)
                    progress.fileURL = url
                    // Cancelling an observer must not retract an already
                    // retained edit or cancel another retry of the mutation.
                    progress.isCancellable = false
                    let entry = Published(item: row.id, direction: row.operation, progress: progress, operation: SocketOperation(progress: progress))
                    published[key] = entry
                    publish(progress)
                }
                published[key]!.operation.report(completed: Int64(row.completed), total: row.total.map(Int64.init),
                    bytesPerSecond: row.bytes_per_second, etaSeconds: row.eta_seconds)
            }
        }
        for key in published.keys.filter({ !present.contains($0) }) {
            unpublish(published.removeValue(forKey: key)!.progress)
        }
    }
}

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
    private var snapshotCursors: [String: String] = [:]
    private var snapshotAnchors: [String: String] = [:]
    private var parents: [String: [String: String]] = [:]
    private var pendingPins: [String: [String: WireItem]] = [:]
    private var queuedParents: [String: Set<String>] = [:]
    private var lastRequests: [String: Date] = [:]
    private var lastSweep = Date.distantPast
    private let transferProgress: NativeBackgroundTransferWatcher

    private let system: NativeSystemManaging
    private let timeout: TimeInterval
    private let now: () -> Date
    init(bridge: BackendBridge, system: NativeSystemManaging = NativeSystemManager(),
         timeout: TimeInterval = 20, now: @escaping () -> Date = Date.init) {
        self.bridge = bridge; self.system = system; self.timeout = timeout; self.now = now
        self.transferProgress = NativeBackgroundTransferWatcher(bridge: bridge, system: system, timeout: min(timeout, 5))
    }
    func stop() { lock.lock(); stopped = true; lock.unlock(); operation.cancel(); transferProgress.stop() }
    func poll(completion: ((Error?) -> Void)? = nil) {
        transferProgress.poll()
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
        var failure: Error?
        for domain in try domains() where smart.contains(domain.identifier.rawValue) {
            try operation.check()
            do { try reconcile(domain) }
            catch {
                // A broken connection must not hide another domain's local
                // changes. Cancellation still ends the complete poll.
                try operation.check()
                if failure == nil { failure = error }
                nativeDevelopmentLog("Native domain \(domain.identifier.rawValue): \(error)")
            }
        }
        if !smart.isEmpty && now().timeIntervalSince(lastSweep) >= 15 {
            do {
                try NativeMaterializationBudget(bridge: bridge, domain: "", system: system, timeout: timeout, now: now).sweep(operation: operation)
                lastSweep = now()
            } catch {
                try operation.check()
                if failure == nil { failure = error }
            }
        }
        if let failure = failure { throw failure }
    }
    private func reconcile(_ domain: NSFileProviderDomain) throws {
        let id = domain.identifier.rawValue
        guard let manager = system.manager(for: domain) else { throw NativeFailure.bridge("A Smart Finder domain manager is unavailable.") }
        let initial = anchors[id] == nil
        let reply: [String: Any]
        do {
            if let anchor = anchors[id] {
                reply = try bridge.request(["op": "changes", "domain": id, "anchor": anchor, "parent_id": "working_set"], operation: operation)
            } else {
                var request: [String: Any] = ["op": "watcher_snapshot", "domain": id]
                request["id"] = snapshotCursors[id]; request["anchor"] = snapshotAnchors[id]
                reply = try bridge.request(request, operation: operation)
            }
        } catch let error as NSError where error.domain == NSFileProviderErrorDomain && error.code == NSFileProviderError.syncAnchorExpired.rawValue {
            anchors.removeValue(forKey: id); snapshotCursors.removeValue(forKey: id); snapshotAnchors.removeValue(forKey: id)
            queuedParents[id, default: []].formUnion((parents[id] ?? [:]).values)
            parents.removeValue(forKey: id); pendingPins.removeValue(forKey: id)
            return
        }
        guard let rows = reply["items"] as? [Any], let anchor = reply["anchor"] as? String else {
            throw NativeFailure.invalid("The Smart catalog omitted its items or anchor.")
        }
        let next = reply["next_id"] as? String
        if initial {
            guard rows.count <= 256, snapshotAnchors[id] == nil || snapshotAnchors[id] == anchor,
                  next == nil || (!next!.isEmpty && next! > (snapshotCursors[id] ?? "")) else {
                throw NativeFailure.invalid("The Smart watcher snapshot page is invalid.")
            }
        }
        let items = try rows.map(WireItem.decode)
        var changedParents = Set<String>()
        if initial && snapshotAnchors[id] == nil {
            changedParents.formUnion((parents[id] ?? [:]).values)
            changedParents.insert("root")
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
        if initial, let next = next {
            snapshotCursors[id] = next; snapshotAnchors[id] = anchor
        } else {
            anchors[id] = anchor
            snapshotCursors.removeValue(forKey: id); snapshotAnchors.removeValue(forKey: id)
        }
        for item in (pendingPins[id] ?? [:]).values { changedParents.insert(item.parent_id) }
        queuedParents[id, default: []].formUnion(changedParents)
        if !items.isEmpty || !(reply["deleted"] as? [String] ?? []).isEmpty || !(queuedParents[id] ?? []).isEmpty {
            manager.signalEnumerator(for: .workingSet) { _ in }
            let pending = queuedParents[id] ?? []
            let ordered = (pending.contains("root") ? ["root"] : []) + pending.filter { $0 != "root" }.sorted()
            for parent in ordered.prefix(256) {
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
}
