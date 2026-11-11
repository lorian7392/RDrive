import Foundation
import FileProvider
import Darwin

/// The native SDK boundary used by cache accounting and provider callbacks.
/// Tests supply a manager without contacting or mutating registered OS domains.
protocol NativeDomainManaging: AnyObject {
    func enumeratorForMaterializedItems() -> NSFileProviderEnumerator
    func getUserVisibleURL(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (URL?, Error?) -> Void)
    func evictItem(identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void)
    func signalEnumerator(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void)
    func signalErrorResolved(_ error: NSError, completionHandler: @escaping (Error?) -> Void)
    func requestDownloadForItem(withIdentifier identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void)
    func temporaryDirectoryURL() throws -> URL
}
final class NativeSystemDomainManager: NativeDomainManaging {
    private let manager: NSFileProviderManager
    init(_ manager: NSFileProviderManager) { self.manager = manager }
    func enumeratorForMaterializedItems() -> NSFileProviderEnumerator { manager.enumeratorForMaterializedItems() }
    func getUserVisibleURL(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (URL?, Error?) -> Void) {
        manager.getUserVisibleURL(for: identifier) { completionHandler($0, $1) }
    }
    func evictItem(identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        manager.evictItem(identifier: identifier) { completionHandler($0) }
    }
    func signalEnumerator(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        manager.signalEnumerator(for: identifier) { completionHandler($0) }
    }
    func signalErrorResolved(_ error: NSError, completionHandler: @escaping (Error?) -> Void) {
        manager.signalErrorResolved(error, completionHandler: completionHandler)
    }
    func requestDownloadForItem(withIdentifier identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        manager.requestDownloadForItem(withIdentifier: identifier, requestedRange: nil, completionHandler: completionHandler)
    }
    func temporaryDirectoryURL() throws -> URL { try manager.temporaryDirectoryURL() }
}

/// Clears only the system's retry backoff for errors the caller has resolved.
/// Retried operations still pass through the broker's original revision and
/// retained-edit checks; this neither changes a queue nor confirms an upload.
func signalNativeSyncErrorsResolved(_ manager: NativeDomainManaging, codes: [NSFileProviderError.Code],
                                   completionHandler: @escaping (Error?) -> Void) {
    guard let code = codes.first else { completionHandler(nil); return }
    manager.signalErrorResolved(NSError(domain: NSFileProviderErrorDomain, code: code.rawValue)) { error in
        if let error = error { completionHandler(error); return }
        signalNativeSyncErrorsResolved(manager, codes: Array(codes.dropFirst()), completionHandler: completionHandler)
    }
}

protocol NativeSystemManaging {
    func domains(completionHandler: @escaping ([NSFileProviderDomain], Error?) -> Void)
    func manager(for domain: NSFileProviderDomain) -> NativeDomainManaging?
}
struct NativeSystemManager: NativeSystemManaging {
    func domains(completionHandler: @escaping ([NSFileProviderDomain], Error?) -> Void) {
        NSFileProviderManager.getDomainsWithCompletionHandler(completionHandler)
    }
    func manager(for domain: NSFileProviderDomain) -> NativeDomainManaging? { NSFileProviderManager(for: domain).map(NativeSystemDomainManager.init) }
}

private struct NativeReservation: Codable {
    let domain: String
    let id: String
    let version: String
    let bytes: UInt64
    let targetSize: UInt64
    let pinPreflight: Bool
    let delivered: Bool
    let expires: TimeInterval
}
private struct NativeUsageSnapshot: Codable { var used: UInt64; var pinned: UInt64; var dirty: UInt64 }
private struct NativeBudgetState: Codable {
    var reservations: [String: NativeReservation] = [:]
    var usage: [String: NativeUsageSnapshot]?
}
private struct NativeResident {
    let domain: String
    let id: String
    let bytes: UInt64
    let accessed: Int64
    let pinned: Bool
    let dirty: Bool
    let protected: Bool
    let version: String?
    let current: Bool
    let remoteVersion: String
    let remoteSize: UInt64
    let knownMaterialized: Bool
    let manager: NativeDomainManaging
}
private final class NativeMaterializedObserver: NSObject, NSFileProviderEnumerationObserver {
    let done = DispatchSemaphore(value: 0)
    var items: [NSFileProviderItem] = []
    var page: NSFileProviderPage?
    var failure: Error?
    var suggestedPageSize: Int { 512 }
    func didEnumerate(_ updatedItems: [NSFileProviderItem]) { items += updatedItems }
    func finishEnumerating(upTo nextPage: NSFileProviderPage?) { page = nextPage; done.signal() }
    func finishEnumeratingWithError(_ error: Error) { failure = error; done.signal() }
}

/// Accounts for the OS-owned content cache using native metadata and eviction.
/// Never removes or truncates any user-visible file.
final class NativeMaterializationBudget {
    private let bridge: BackendBridge
    private let domain: String
    private let system: NativeSystemManaging
    private let timeout: TimeInterval
    private let now: () -> Date
    init(bridge: BackendBridge, domain: String, system: NativeSystemManaging = NativeSystemManager(),
         timeout: TimeInterval = 20, now: @escaping () -> Date = Date.init) {
        self.bridge = bridge; self.domain = domain; self.system = system; self.timeout = timeout; self.now = now
    }

    private func quota(_ message: String) -> NSError {
        NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.insufficientQuota.rawValue,
                userInfo: [NSLocalizedDescriptionKey: message])
    }
    private func sum(_ values: [UInt64]) throws -> UInt64 {
        var total: UInt64 = 0
        for value in values {
            let next = total.addingReportingOverflow(value)
            guard !next.overflow else { throw quota("Cache usage exceeds the supported size.") }
            total = next.partialValue
        }
        return total
    }
    private func key(_ domain: String, _ id: String) -> String { Data((domain + "\n" + id).utf8).base64EncodedString() }

    private func locked<T>(_ operation: SocketOperation?, _ body: (inout NativeBudgetState) throws -> T) throws -> T {
        let path = bridge.paths.group.appendingPathComponent("native-budget.lock")
        let fd = open(path.path, O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard fd >= 0 else { throw NativeFailure.bridge("The native cache budget lock is unavailable.") }
        defer { _ = flock(fd, LOCK_UN); close(fd) }
        var metadata = stat()
        guard fstat(fd, &metadata) == 0, metadata.st_uid == getuid(), metadata.st_nlink == 1,
              metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_mode & 0o077 == 0 else {
            throw NativeFailure.invalid("The native cache budget lock must be a private regular file.")
        }
        let deadline = now().addingTimeInterval(timeout)
        while flock(fd, LOCK_EX | LOCK_NB) != 0 {
            try operation?.check()
            guard errno == EWOULDBLOCK, now() < deadline else { throw NativeFailure.bridge("Another native cache operation is still in progress.") }
            usleep(20_000)
        }
        let stateURL = bridge.paths.group.appendingPathComponent("native-budget.json")
        var state = NativeBudgetState()
        let stateFD = open(stateURL.path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
        if stateFD >= 0 {
            let handle = FileHandle(fileDescriptor: stateFD, closeOnDealloc: true)
            var info = stat()
            guard fstat(stateFD, &info) == 0, info.st_uid == getuid(), info.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
                  info.st_mode & 0o077 == 0, info.st_size <= 1024 * 1024 else { throw NativeFailure.invalid("Invalid native cache reservation file.") }
            state = try JSONDecoder().decode(NativeBudgetState.self, from: handle.readDataToEndOfFile())
        } else if errno != ENOENT { throw NativeFailure.bridge("Native cache reservations cannot be read safely.") }
        state.reservations = state.reservations.filter { $0.value.expires > now().timeIntervalSince1970 }
        let result = try body(&state)
        let payload = try JSONEncoder().encode(state)
        guard payload.count <= 1024 * 1024 else { throw NativeFailure.invalid("The native reservation plan is too large; pin smaller folders separately.") }
        try payload.write(to: stateURL, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: stateURL.path)
        let committed = open(stateURL.path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
        guard committed >= 0 else { throw NativeFailure.bridge("The native cache reservation could not be committed.") }
        defer { close(committed) }
        guard fsync(committed) == 0 else { throw NativeFailure.bridge("The native cache reservation could not be saved durably.") }
        return result
    }

    private func awaitValue<T>(_ body: (@escaping (T?, Error?) -> Void) -> Void) throws -> T {
        let done = DispatchSemaphore(value: 0)
        var value: T?; var failure: Error?
        body { value = $0; failure = $1; done.signal() }
        guard done.wait(timeout: .now() + timeout) == .success else { throw NativeFailure.bridge("The system cache metadata request timed out.") }
        if let failure = failure { throw failure }
        guard let value = value else { throw NativeFailure.bridge("The system did not return cache metadata.") }
        return value
    }

    private func residents(_ operation: SocketOperation?) throws -> [NativeResident] {
        let reply = try bridge.request(["op": "domains"], operation: operation)
        let smart = Set((reply["domains"] as? [[String: Any]] ?? []).filter { $0["smart"] as? Bool == true }.compactMap { $0["id"] as? String })
        let domains: [NSFileProviderDomain] = try awaitValue { self.system.domains(completionHandler: $0) }
        var result: [NativeResident] = []
        for domain in domains where smart.contains(domain.identifier.rawValue) {
            guard let manager = system.manager(for: domain) else { throw NativeFailure.bridge("A registered Smart domain's cache cannot be inspected safely.") }
            let enumerator = manager.enumeratorForMaterializedItems()
            defer { enumerator.invalidate() }
            var page: NSFileProviderPage? = NSFileProviderPage(Data())
            var seen = Set<String>()
            while let current = page {
                try operation?.check()
                let observer = NativeMaterializedObserver()
                enumerator.enumerateItems(for: observer, startingAt: current)
                guard observer.done.wait(timeout: .now() + timeout) == .success else { throw NativeFailure.bridge("The system materialized-file scan timed out.") }
                if let failure = observer.failure { throw failure }
                for item in observer.items {
                    let id = wireID(item.itemIdentifier)
                    guard id != "root", seen.insert(id).inserted else { continue }
                    guard seen.count <= 100000 else { throw NativeFailure.bridge("The materialized-file scan is too large to account safely.") }
                    let url: URL = try awaitValue { manager.getUserVisibleURL(for: item.itemIdentifier, completionHandler: $0) }
                    let scoped = url.startAccessingSecurityScopedResource()
                    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                    var info = stat()
                    guard lstat(url.path, &info) == 0 else {
                        if errno == ENOENT { continue }
                        throw NativeFailure.bridge("A materialized file's size could not be checked.")
                    }
                    guard info.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), info.st_flags & UInt32(SF_DATALESS) == 0 else { continue }
                    let wire = try bridge.item(["op": "item", "domain": domain.identifier.rawValue, "id": id], operation: operation)
                    let state = NativeSyncState.item(wire)
                    let dirty = wire.has_unsynced_changes ?? [.queued, .conflict, .syncing].contains(state)
                    result.append(NativeResident(domain: domain.identifier.rawValue, id: id, bytes: UInt64(max(0, info.st_size)),
                        accessed: Int64(info.st_atimespec.tv_sec), pinned: wire.pinned,
                        dirty: dirty,
                        protected: dirty || state == .error,
                        version: item.itemVersion.flatMap { String(data: $0.contentVersion, encoding: .utf8) },
                        current: item.isMostRecentVersionDownloaded == true,
                        remoteVersion: wire.content_version, remoteSize: wire.size, knownMaterialized: wire.native_materialized == true, manager: manager))
                }
                page = observer.page
            }
        }
        return result
    }

    private func evict(_ resident: NativeResident) -> Bool {
        let done = DispatchSemaphore(value: 0); var failure: Error?
        resident.manager.evictItem(identifier: providerID(resident.id)) { failure = $0; done.signal() }
        return done.wait(timeout: .now() + timeout) == .success && failure == nil
    }

    /// Reservation survives the callback until the OS materializes its visible file.
    /// A later scan reconciles it; failed/cancelled deliveries release it explicitly.
    func reserve(_ item: WireItem, pin: Bool = false, operation: SocketOperation?) throws -> [String]? {
        guard item.smart == true else { return nil }
        let budget = try bridge.request(["op": "cache_budget"], operation: operation)
        let limit = (budget["max_bytes"] as? NSNumber)?.uint64Value ?? UInt64.max
        var selected = [item]
        if item.kind == "directory" {
            let reply = try bridge.request(["op": "list", "domain": domain, "parent_id": "working_set"], operation: operation)
            let all = try (reply["items"] as? [Any] ?? []).map(WireItem.decode)
            guard all.count <= 100000 else { throw quota("This folder is too large to preflight safely. Pin a smaller folder or individual files.") }
            let children = Dictionary(grouping: all, by: { $0.parent_id })
            var pending = [item.id]; var seen = Set<String>(); selected = []
            while let parent = pending.popLast() {
                guard seen.insert(parent).inserted else { throw NativeFailure.invalid("The folder catalog contains a cycle.") }
                for child in children[parent] ?? [] {
                    if child.kind == "directory" { pending.append(child.id) } else { selected.append(child) }
                }
            }
        }
        guard try sum(selected.map { $0.size }) <= limit else { throw quota("The selected content is larger than the configured cache budget. Increase the budget before downloading or keeping it offline.") }
        return try account(items: selected, pin: pin, limit: limit, operation: operation)
    }

    func sweep(operation: SocketOperation? = nil) throws {
        let domains = try bridge.request(["op": "domains"], operation: operation)["domains"] as? [[String: Any]] ?? []
        guard domains.contains(where: { $0["smart"] as? Bool == true }) else { return }
        let budget = try bridge.request(["op": "cache_budget"], operation: operation)
        let limit = (budget["max_bytes"] as? NSNumber)?.uint64Value ?? UInt64.max
        _ = try account(items: nil, limit: limit, operation: operation)
    }

    func trim(to limit: UInt64, operation: SocketOperation? = nil) throws {
        _ = try account(items: nil, limit: limit, operation: operation)
    }

    private func account(items: [WireItem]?, pin: Bool = false, limit: UInt64, operation: SocketOperation?) throws -> [String]? {
        try locked(operation) { state in
            let configured = try bridge.request(["op": "domains"], operation: operation)
            let allowed = Set((configured["domains"] as? [[String: Any]] ?? []).filter { $0["smart"] as? Bool == true }.compactMap { $0["id"] as? String })
            let registered: [NSFileProviderDomain] = try awaitValue { self.system.domains(completionHandler: $0) }
            let registeredIDs = Set(registered.map { $0.identifier.rawValue })
            for removed in (state.usage ?? [:]).keys where !allowed.contains(removed) && !registeredIDs.contains(removed) {
                // Only OS-confirmed absence permits clearing durable used bytes.
                // Preserve counters when a domain is merely removed from config.
                _ = try bridge.request(["op": "cache_budget", "domain": removed, "native_accounting_id": "fileprovider:" + removed,
                    "native_used_bytes": 0, "native_reserved_bytes": 0, "native_pinned_bytes": 0, "native_dirty_bytes": 0,
                    "native_confirmed_unregistered": true], operation: operation)
            }
            state.reservations = state.reservations.filter { allowed.contains($0.value.domain) || registeredIDs.contains($0.value.domain) }
            state.usage = (state.usage ?? [:]).filter { allowed.contains($0.key) || registeredIDs.contains($0.key) }
            var files = try residents(operation)
            let receipts = Array(state.reservations.values.filter { $0.delivered })
            state.reservations = state.reservations.filter { _, reservation in
                !reservation.delivered || !files.contains(where: {
                    $0.domain == reservation.domain && $0.id == reservation.id && !$0.dirty && $0.bytes == reservation.targetSize
                })
            }
            func snapshot() throws -> (UInt64, [String: Any]) {
                let domains = Set(files.map { $0.domain } + state.reservations.values.map { $0.domain } + Array((state.usage ?? [:]).keys)).union(allowed)
                var reply: [String: Any] = [:]
                for current in domains {
                    let own = files.filter { $0.domain == current }
                    let previous = state.usage?[current]
                    let used = try allowed.contains(current) ? sum(own.filter { !$0.dirty }.map { $0.bytes }) : (previous?.used ?? 0)
                    let pinned = try allowed.contains(current) ? sum(own.filter { !$0.dirty && $0.pinned }.map { $0.bytes }) : (previous?.pinned ?? 0)
                    let dirty = try allowed.contains(current) ? sum(own.filter { $0.dirty }.map { $0.bytes }) : (previous?.dirty ?? 0)
                    let versions = Dictionary(uniqueKeysWithValues: own.filter { file in
                        !file.dirty && file.bytes == file.remoteSize && ((file.current && file.version == file.remoteVersion) ||
                        file.knownMaterialized || receipts.contains(where: { receipt in
                            receipt.domain == current && receipt.id == file.id && receipt.version == file.remoteVersion && receipt.targetSize == file.bytes
                        }))
                    }.map { ($0.id, $0.remoteVersion) })
                    state.usage = state.usage ?? [:]
                    state.usage?[current] = NativeUsageSnapshot(used: used, pinned: pinned, dirty: dirty)
                    let reserved = try sum(state.reservations.values.filter { $0.domain == current }.map { $0.bytes })
                    var message: [String: Any] = ["op": "cache_budget", "domain": current, "native_accounting_id": "fileprovider:" + current,
                        "native_used_bytes": used, "native_reserved_bytes": reserved, "native_pinned_bytes": pinned,
                        "native_dirty_bytes": dirty]
                    if allowed.contains(current) {
                        let entries = versions.sorted { $0.key < $1.key }
                        for start in stride(from: 0, to: max(entries.count, 1), by: 512) {
                            let end = min(start + 512, entries.count)
                            message["native_cached_versions"] = Dictionary(uniqueKeysWithValues: entries[start..<end].map { ($0.key, $0.value) })
                            message["native_cached_versions_reset"] = start == 0
                            reply = try bridge.request(message, operation: operation)
                        }
                    } else { reply = try bridge.request(message, operation: operation) }
                }
                if domains.isEmpty { reply = try bridge.request(["op": "cache_budget"], operation: operation) }
                guard let used = (reply["used_bytes"] as? NSNumber)?.uint64Value,
                      let reserved = (reply["reserved_bytes"] as? NSNumber)?.uint64Value else {
                    throw NativeFailure.invalid("The combined cache accounting reply is incomplete.")
                }
                return (try sum([used, reserved]), reply)
            }
            // Reconcile/report current usage first. The Rust ledger deliberately
            // refuses an over-budget increase in reservations, so victims must
            // be evicted before submitting the new reservation grant.
            var baselineTotal = try snapshot().0
            let previous = state.reservations
            var proposed = previous
            var requested: [String]? = items == nil ? nil : []
            let requestedItems = Set((items ?? []).map { key(domain, $0.id) })
            for item in items ?? [] {
                if pin && proposed.values.contains(where: { $0.domain == domain && $0.id == item.id &&
                    $0.version == item.content_version && $0.targetSize == item.size }) {
                    continue // No shared ownership: callers receive no token for another owner's lease.
                }
                if !pin {
                    proposed = proposed.filter { !($0.value.pinPreflight && $0.value.domain == domain && $0.value.id == item.id && $0.value.version == item.content_version) }
                }
                let resident = files.first { $0.domain == domain && $0.id == item.id && !$0.dirty }
                let delta = item.size > (resident?.bytes ?? 0) ? item.size - (resident?.bytes ?? 0) : 0
                let token = UUID().uuidString
                proposed[token] = NativeReservation(domain: domain, id: item.id, version: item.content_version,
                    bytes: delta, targetSize: item.size, pinPreflight: pin, delivered: false, expires: now().timeIntervalSince1970 + 120)
                requested?.append(token)
            }
            let oldReserved = try sum(previous.values.map { $0.bytes })
            let nextReserved = try sum(proposed.values.map { $0.bytes })
            var plan = state; plan.reservations = proposed
            guard try JSONEncoder().encode(plan).count <= 1024 * 1024 else {
                throw NativeFailure.invalid("The native reservation plan is too large; pin smaller folders separately.")
            }
            func projected(_ baseline: UInt64) throws -> UInt64 {
                try sum([baseline >= oldReserved ? baseline - oldReserved : 0, nextReserved])
            }
            var total = try projected(baselineTotal)
            for victim in files.sorted(by: { $0.accessed < $1.accessed }) where total > limit {
                try operation?.check()
                guard !victim.pinned, !victim.protected, !requestedItems.contains(key(victim.domain, victim.id)) else { continue }
                if evict(victim) {
                    files.removeAll { $0.domain == victim.domain && $0.id == victim.id }
                    baselineTotal = try snapshot().0
                    total = try projected(baselineTotal)
                }
            }
            guard total <= limit else {
                throw quota("The cache budget is occupied by kept-offline, unsynced, or currently used files. Release eligible downloads or increase the budget; no protected file was removed.")
            }
            state.reservations = proposed
            do { _ = try snapshot() }
            catch {
                state.reservations = previous
                _ = try? snapshot()
                throw error // A concurrent Rust admission can win; never overbook.
            }
            return requested
        }
    }

    func release(_ reservation: [String]?) {
        guard let reservation = reservation else { return }
        _ = try? locked(nil) { state in
            var domains = Set<String>()
            for key in reservation {
                if let removed = state.reservations.removeValue(forKey: key) { domains.insert(removed.domain) }
            }
            for domain in domains { try publish(domain, state) }
        }
    }

    private func publish(_ current: String, _ state: NativeBudgetState) throws {
        let usage = state.usage?[current] ?? NativeUsageSnapshot(used: 0, pinned: 0, dirty: 0)
        let reserved = try sum(state.reservations.values.filter { $0.domain == current }.map { $0.bytes })
        _ = try bridge.request(["op": "cache_budget", "domain": current, "native_accounting_id": "fileprovider:" + current,
            "native_used_bytes": usage.used, "native_reserved_bytes": reserved, "native_pinned_bytes": usage.pinned,
            "native_dirty_bytes": usage.dirty])
    }

    func renew(_ reservation: [String]?) throws {
        guard let reservation = reservation else { return }
        try locked(nil) { state in
            var domains = Set<String>()
            for key in reservation {
                guard let current = state.reservations[key] else { continue }
                state.reservations[key] = NativeReservation(domain: current.domain, id: current.id, version: current.version,
                    bytes: current.bytes, targetSize: current.targetSize, pinPreflight: current.pinPreflight,
                    delivered: current.delivered, expires: now().timeIntervalSince1970 + 120)
                domains.insert(current.domain)
            }
            for domain in domains { try publish(domain, state) }
        }
    }

    func markDelivered(_ reservation: [String]?) throws {
        guard let reservation = reservation else { return }
        try locked(nil) { state in
            for key in reservation {
                guard let current = state.reservations[key] else { continue }
                state.reservations[key] = NativeReservation(domain: current.domain, id: current.id, version: current.version,
                    bytes: current.bytes, targetSize: current.targetSize, pinPreflight: false, delivered: true,
                    expires: now().timeIntervalSince1970 + 120)
            }
        }
    }

    /// A successful native create/content-modify response confirms bytes that
    /// already exist in the user's local file. This is a status receipt, not
    /// extra cache allocation or an acknowledgement of a pending remote edit.
    func acknowledgeLocalContent(_ item: WireItem) throws {
        guard item.smart == true, item.kind == "file", item.has_unsynced_changes != true else { return }
        try locked(nil) { state in
            state.reservations[UUID().uuidString] = NativeReservation(domain: domain, id: item.id,
                version: item.content_version, bytes: 0, targetSize: item.size, pinPreflight: false,
                delivered: true, expires: now().timeIntervalSince1970 + 120)
        }
    }
}
