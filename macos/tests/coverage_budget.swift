import Foundation
import FileProvider
import Darwin

private let budgetFailure = NSError(domain: "RDriveSyntheticNative", code: 17)

final class BudgetPeer {
    let paths: NativePaths
    private let listener: Int32
    private let done = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var stopped = false
    var configured: [[String: Any]] = [["id": "budget-domain", "smart": true]]
    var catalog: [[String: Any]] = []
    var items: [String: [String: Any]] = [:]
    var limit: UInt64? = 1024
    var calls: [[String: Any]] = []
    var ledger: [String: [String: UInt64]] = [:]
    var expireChanges = false
    var malformedCatalog = false
    var malformedDomains = Set<String>()
    var catalogByDomain: [String: [[String: Any]]] = [:]
    var snapshotReply: [String: Any]?
    var blockedCatalogDomain: String?
    let catalogStarted = DispatchSemaphore(value: 0)
    var incompleteBudget = false
    var rejectReservation = false
    var extraUsed: UInt64 = 0
    let fetchStarted = DispatchSemaphore(value: 0)
    let fetchFinished = DispatchSemaphore(value: 0)
    let budgetRenewed = DispatchSemaphore(value: 0)
    private let fetchWorkers = DispatchGroup()
    private var activeFetches = 0
    var fetchGate: DispatchSemaphore?
    var fetchWaitForCancellation = false
    var fetchFailure = false
    var fetchDelay: TimeInterval = 0

    init(root: URL) throws {
        let group = root.appendingPathComponent("budget-" + String(UUID().uuidString.prefix(8)))
        try FileManager.default.createDirectory(at: group, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        paths = NativePaths(group: group)
        listener = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard listener >= 0 else { throw budgetFailure }
        // Teardown can reject queued connections before accept configures
        // them; protect the listening socket as well as each accepted socket.
        var one: Int32 = 1
        guard setsockopt(listener, SOL_SOCKET, SO_NOSIGPIPE, &one,
                         socklen_t(MemoryLayout.size(ofValue: one))) == 0 else {
            Darwin.close(listener)
            throw budgetFailure
        }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let path = Array(paths.socket.path.utf8) + [0]
        guard path.count <= MemoryLayout.size(ofValue: address.sun_path) else { throw budgetFailure }
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: path) }
        let bound = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(listener, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard bound == 0 && Darwin.listen(listener, 8) == 0 else { throw budgetFailure }
        DispatchQueue.global(qos: .utility).async { self.serve() }
    }
    func mutate(_ body: (BudgetPeer) -> Void) { lock.lock(); body(self); lock.unlock() }
    func inspect<T>(_ body: (BudgetPeer) -> T) -> T { lock.lock(); defer { lock.unlock() }; return body(self) }
    func listenerSuppressesSIGPIPE() throws -> Bool {
        var value: Int32 = 0
        var size = socklen_t(MemoryLayout.size(ofValue: value))
        guard getsockopt(listener, SOL_SOCKET, SO_NOSIGPIPE, &value, &size) == 0 else { throw budgetFailure }
        return value == 1
    }
    func stop() {
        lock.lock(); stopped = true; let gate = fetchGate; lock.unlock()
        gate?.signal()
        _ = Darwin.shutdown(listener, SHUT_RDWR); Darwin.close(listener)
        _ = done.wait(timeout: .now() + 2)
        _ = fetchWorkers.wait(timeout: .now() + 2)
    }
    private func reply(_ request: [String: Any]) -> [String: Any] {
        lock.lock(); defer { lock.unlock() }
        calls.append(request)
        switch request["op"] as? String {
        case "domains": return ["ok": true, "domains": configured]
        case "transfer_status": return ["ok": true, "transfers": []]
        case "item":
            if let id = request["id"] as? String, let item = items[id] { return ["ok": true, "item": item] }
            return ["ok": false, "error": ["code": "no_such_item"]]
        case "fetch":
            if fetchFailure { return ["ok": false, "error": ["code": "server_unreachable"]] }
            let transfers=paths.transfers
            try! FileManager.default.createDirectory(at:transfers,withIntermediateDirectories:true,attributes:[.posixPermissions:0o700])
            let staged=transfers.appendingPathComponent(UUID().uuidString)
            try! Data("provider-content".utf8).write(to:staged)
            return ["ok":true,"item":items[request["id"] as! String]!,"content_path":staged.path]
        case "native_error", "pin":return ["ok":true]
        case "list", "changes", "watcher_snapshot":
            if request["op"] as? String == "changes" && expireChanges {
                expireChanges = false; return ["ok": false, "error": ["code": "sync_anchor_expired"]]
            }
            let domain = request["domain"] as? String ?? ""
            if malformedCatalog || malformedDomains.contains(domain) { return ["ok": true] }
            let values = catalogByDomain[domain] ?? catalog
            if request["op"] as? String == "watcher_snapshot" {
                if let override = snapshotReply { return override }
                let cursor = request["id"] as? String ?? ""
                let remaining = values.sorted { ($0["id"] as! String) < ($1["id"] as! String) }.filter { ($0["id"] as! String) > cursor }
                let page = Array(remaining.prefix(256))
                var response: [String: Any] = ["ok": true, "items": page, "anchor": request["anchor"] as? String ?? "next"]
                if remaining.count > page.count { response["next_id"] = page.last!["id"] }
                return response
            }
            return ["ok": true, "items": values, "deleted": ["removed-file"], "anchor": "next"]
        case "cache_budget":
            if incompleteBudget { return ["ok": true] }
            if let domain = request["domain"] as? String, let used = request["native_used_bytes"] as? NSNumber {
                let reserved = (request["native_reserved_bytes"] as? NSNumber)?.uint64Value ?? 0
                if rejectReservation && reserved > 0 {
                    rejectReservation = false
                    return ["ok": false, "error": ["code": "insufficient_quota"]]
                }
                ledger[domain] = ["used": used.uint64Value, "reserved": reserved]
                if activeFetches > 0 && reserved > 0 { budgetRenewed.signal() }
            }
            let used = ledger.values.reduce(extraUsed) { $0 + ($1["used"] ?? 0) }
            let reserved = ledger.values.reduce(UInt64(0)) { $0 + ($1["reserved"] ?? 0) }
            var response: [String: Any] = ["ok": true, "used_bytes": used, "reserved_bytes": reserved]
            if let limit = limit { response["max_bytes"] = limit }
            return response
        default: return ["ok": false, "error": ["code": "unsupported"]]
        }
    }
    private func serve() {
        defer { done.signal() }
        while true {
            lock.lock(); let closed = stopped; lock.unlock()
            if closed { return }
            let fd = Darwin.accept(listener, nil, nil)
            if fd < 0 { return }
            var ownedDescriptor = fd
            defer { if ownedDescriptor >= 0 { Darwin.close(ownedDescriptor) } }
            var one: Int32 = 1
            _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout.size(ofValue: one)))
            var raw = Data(); var buffer = [UInt8](repeating: 0, count: 65536)
            while !raw.contains(10) {
                let count = Darwin.read(fd, &buffer, buffer.count)
                if count <= 0 { break }
                raw.append(contentsOf: buffer.prefix(count))
            }
            guard let newline = raw.firstIndex(of: 10), let request = try? JSONSerialization.jsonObject(with: raw.prefix(upTo: newline)) as? [String: Any] else { continue }
            if request["op"] as? String == "fetch" {
                let (gate, delay, waitForCancellation) = inspect { ($0.fetchGate, $0.fetchDelay, $0.fetchWaitForCancellation) }
                mutate { $0.activeFetches += 1 }
                fetchStarted.signal()
                ownedDescriptor = -1
                fetchWorkers.enter()
                // A held content response must not block independent lease
                // renewals or keep the fixture's shared-state lock occupied.
                DispatchQueue.global(qos: .userInitiated).async {
                    defer {
                        Darwin.close(fd)
                        self.mutate { $0.activeFetches -= 1 }
                        self.fetchFinished.signal()
                        self.fetchWorkers.leave()
                    }
                    if waitForCancellation {
                        var byte: UInt8 = 0
                        while Darwin.read(fd, &byte, 1) > 0 { }
                        return
                    }
                    if let gate = gate, gate.wait(timeout: .now() + 10) != .success {
                        self.send(["ok": false, "error": ["code": "server_unreachable", "message": "Synthetic fetch gate timed out"]], to: fd)
                        return
                    }
                    if delay > 0 { Thread.sleep(forTimeInterval: delay) }
                    self.send(self.reply(request), to: fd)
                }
                continue
            }
            if ["watcher_snapshot", "changes"].contains(request["op"] as? String ?? ""),
               inspect({ $0.blockedCatalogDomain }) == request["domain"] as? String {
                mutate { $0.calls.append(request) }; catalogStarted.signal()
                // A deliberately blocked request acknowledges cancellation by
                // reading EOF, just as the main IPC cancellation fixture does.
                while Darwin.read(fd, &buffer, buffer.count) > 0 { }
                continue
            }
            send(reply(request), to: fd)
        }
    }
    private func send(_ response: [String: Any], to fd: Int32) {
        guard var payload = try? JSONSerialization.data(withJSONObject: response) else { return }
        payload.append(10)
        payload.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                if count <= 0 { break }; offset += count
            }
        }
    }
}

private final class BudgetMaterialized: NSObject, NSFileProviderItem {
    let wire: WireItem
    let current: Bool
    init(_ wire: WireItem, current: Bool = true) { self.wire = wire; self.current = current }
    var itemIdentifier: NSFileProviderItemIdentifier { providerID(wire.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { providerID(wire.parent_id) }
    var filename: String { wire.name }
    var itemVersion: NSFileProviderItemVersion { NSFileProviderItemVersion(contentVersion: Data(wire.content_version.utf8), metadataVersion: Data()) }
    var isMostRecentVersionDownloaded: Bool { current }
}
private final class BudgetEnumeration: NSObject, NSFileProviderEnumerator {
    let items: [NSFileProviderItem]
    let error: Error?
    let silent: Bool
    var invalidated = false
    init(items: [NSFileProviderItem], error: Error?, silent: Bool) { self.items = items; self.error = error; self.silent = silent }
    func invalidate() { invalidated = true }
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        if silent { return }
        if let error = error { observer.finishEnumeratingWithError(error); return }
        let offset = Int(String(data: page.rawValue, encoding: .utf8) ?? "") ?? 0
        let pageSize=max(1,min(observer.suggestedPageSize ?? 2,2))
        observer.didEnumerate(Array(items.dropFirst(offset).prefix(pageSize)))
        observer.finishEnumerating(upTo: offset + pageSize < items.count ? NSFileProviderPage(Data(String(offset + pageSize).utf8)) : nil)
    }
}
private final class BudgetManager: NativeDomainManaging {
    var items: [NSFileProviderItem] = []
    var urls: [String: URL] = [:]
    var evicted: [String] = []
    var signalled: [String] = []
    var downloads: [String] = []
    var enumerationError: Error?
    var enumerationSilent = false
    var urlError: Error?
    var urlSilent = false
    var evictionError: Error?
    var evictionSilent = false
    var signalError: Error?
    let temporary: URL
    init(_ temporary: URL) { self.temporary = temporary }
    func enumeratorForMaterializedItems() -> NSFileProviderEnumerator {
        BudgetEnumeration(items: items, error: enumerationError, silent: enumerationSilent)
    }
    func getUserVisibleURL(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (URL?, Error?) -> Void) {
        if !urlSilent { completionHandler(urls[wireID(identifier)], urlError) }
    }
    func evictItem(identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        evicted.append(wireID(identifier))
        if evictionError == nil && !evictionSilent { items.removeAll { $0.itemIdentifier == identifier } }
        if !evictionSilent { completionHandler(evictionError) }
    }
    func signalEnumerator(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { signalled.append(wireID(identifier)); completionHandler(signalError) }
    func signalErrorResolved(_ error: NSError, completionHandler: @escaping (Error?) -> Void) { completionHandler(nil) }
    func requestDownloadForItem(withIdentifier identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { downloads.append(wireID(identifier)); completionHandler(nil) }
    func temporaryDirectoryURL() throws -> URL { temporary }
}
private final class BudgetSystem: NativeSystemManaging {
    var registered = [NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("budget-domain"), displayName: "Synthetic Budget")]
    var managerValue: BudgetManager?
    var domainManagers: [String: BudgetManager] = [:]
    var missingManagers = Set<String>()
    var error: Error?
    var silent = false
    init(_ manager: BudgetManager?) { managerValue = manager }
    func domains(completionHandler: @escaping ([NSFileProviderDomain], Error?) -> Void) { if !silent { completionHandler(registered, error) } }
    func manager(for domain: NSFileProviderDomain) -> NativeDomainManaging? {
        missingManagers.contains(domain.identifier.rawValue) ? nil : domainManagers[domain.identifier.rawValue] ?? managerValue
    }
}

enum BudgetCoverageTests {
    private static func item(_ id: String, size: UInt64 = 8, parent: String = "root", kind: String = "file", pinned: Bool = false, dirty: Bool = false, state: String = "cached") throws -> [String: Any] {
        ["id": id, "parent_id": parent, "name": id + ".txt", "kind": kind, "size": size,
         "content_version": "v1", "metadata_version": "m1", "pinned": pinned,
         "capabilities": ["read"], "smart": true, "has_unsynced_changes": dirty, "sync_state": state]
    }
    private static func poll(_ watcher: NativeDomainWatcher) throws -> Error? {
        let done = DispatchSemaphore(value: 0); var error: Error?
        watcher.poll { error = $0; done.signal() }
        try wait(done); return error
    }
    static func run(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        let peer = try BudgetPeer(root: paths.group); defer { peer.stop() }
        let manager = BudgetManager(peer.paths.group)
        let system = BudgetSystem(manager)
        let own = BackendBridge(paths: peer.paths)
        var now = Date()
        func budget() -> NativeMaterializationBudget {
            NativeMaterializationBudget(bridge: own, domain: "budget-domain", system: system, timeout: 0.02, now: { now })
        }
        func resetState() throws {
            try? FileManager.default.removeItem(at: peer.paths.group.appendingPathComponent("native-budget.json"))
            peer.mutate { $0.ledger = [:]; $0.calls = []; $0.extraUsed = 0 }
        }
        func reservations() throws -> [String: [String: Any]] {
            let data = try Data(contentsOf: peer.paths.group.appendingPathComponent("native-budget.json"))
            return (try JSONSerialization.jsonObject(with: data) as! [String: Any])["reservations"] as! [String: [String: Any]]
        }
        let requested = try item("requested", size: 10)
        let request = try WireItem.decode(requested)
        peer.mutate { $0.items["requested"] = requested }
        let first = try budget().reserve(request, pin: true, operation: nil)!
        try expect(first.count == 1, "Pin preflight did not allocate one independently owned lease")
        let repeated = try budget().reserve(request, pin: true, operation: nil)!
        try expect(repeated.isEmpty, "Repeated pin acquired another owner's lease")
        let fetch = try budget().reserve(request, operation: nil)!
        try expect(fetch.count == 1 && Set(fetch).isDisjoint(with: first), "Fetch did not replace preflight with its own lease")
        let reservation = try reservations()[fetch[0]]!
        try expect((reservation["bytes"] as? NSNumber)?.intValue == 10, "Reservation bytes incorrect")
        now.addTimeInterval(30)
        try budget().renew(fetch); try budget().markDelivered(fetch)
        try expect(try reservations()[fetch[0]]?["delivered"] as? Bool == true, "Delivered lease lost its receipt")
        budget().release(fetch)
        try expect(try reservations().isEmpty, "Lease was not released")
        let stale = try budget().reserve(request, operation: nil)!
        now.addTimeInterval(121)
        try budget().renew([])
        try expect(try reservations()[stale[0]] == nil, "Expired reservation survived reconciliation")

        let clean = try item("clean", size: 8)
        let pinned = try item("pinned", size: 9, pinned: true)
        let dirty = try item("dirty", size: 7, dirty: true, state: "queued")
        let error = try item("error", size: 6, state: "error")
        let absent = try item("absent")
        let directory = try item("directory", kind: "directory")
        for raw in [clean, pinned, dirty, error, absent, directory] {
            let wire = try WireItem.decode(raw)
            let file = peer.paths.group.appendingPathComponent(wire.id)
            if wire.id == "directory" { try FileManager.default.createDirectory(at: file, withIntermediateDirectories: false) }
            else if wire.id != "absent" { try Data(repeating: 1, count: Int(wire.size)).write(to: file) }
            manager.urls[wire.id] = file
            manager.items.append(BudgetMaterialized(wire))
            peer.mutate { $0.items[wire.id] = raw }
        }
        manager.items += [BudgetMaterialized(try WireItem.decode(clean)), BudgetMaterialized(try WireItem.decode(item("root", kind: "directory")))]
        try resetState()
        try budget().trim(to: 15)
        try expect(manager.evicted == ["clean"], "LRU evicted pinned, dirty, erroneous or duplicate objects")
        try expect(FileManager.default.fileExists(atPath: manager.urls["clean"]!.path), "Mock eviction unexpectedly removed real synthetic content")
        try expect(peer.inspect { $0.ledger["budget-domain"]?["used"] } == 15, "Confirmed native clean bytes were not reconciled")
        try NativeTests.rejects("Protected content was silently discarded") { try budget().trim(to: 0) }
        try expect(manager.evicted == ["clean"], "Protected file was offered for eviction")
        manager.items = []
        try resetState()
        peer.mutate { $0.rejectReservation = true }
        try NativeTests.rejects("Failed concurrent admission was accepted") { _ = try budget().reserve(request, operation: nil) }
        try expect(peer.inspect { $0.ledger["budget-domain"]?["reserved"] } == 0, "Rejected admission was not rolled back")
        peer.mutate { $0.incompleteBudget = true }
        try NativeTests.rejects("Incomplete combined accounting was used") { try budget().trim(to: 0) }
        peer.mutate { $0.incompleteBudget = false }
        system.error = budgetFailure
        try NativeTests.rejects("Domain-list SDK failure was ignored") { try budget().trim(to: 0) }
        system.error = nil; system.silent = true
        try NativeTests.rejects("Silent domain list did not time out") { try budget().trim(to: 0) }
        system.silent = false; system.managerValue = nil
        try NativeTests.rejects("Missing native manager was interpreted as zero usage") { try budget().trim(to: 0) }
        system.managerValue = manager
        manager.enumerationError = budgetFailure
        try NativeTests.rejects("Materialized scan error was ignored") { try budget().trim(to: 0) }
        manager.enumerationError = nil; manager.enumerationSilent = true
        try NativeTests.rejects("Silent materialized scan did not time out") { try budget().trim(to: 0) }
        manager.enumerationSilent = false
        manager.items = [BudgetMaterialized(try WireItem.decode(clean))]
        manager.urlError = budgetFailure
        try NativeTests.rejects("URL SDK failure was ignored") { try budget().trim(to: 0) }
        manager.urlError = nil; manager.urlSilent = true
        try NativeTests.rejects("Silent URL lookup did not time out") { try budget().trim(to: 0) }
        manager.urlSilent = false; manager.urls["clean"] = nil
        try NativeTests.rejects("Missing URL was used as a resident") { try budget().trim(to: 0) }
        manager.urls["clean"] = peer.paths.group.appendingPathComponent("clean")
        manager.evictionError = budgetFailure
        try NativeTests.rejects("Native eviction refusal was ignored") { try budget().trim(to: 0) }
        manager.evictionError = nil; manager.evictionSilent = true
        try NativeTests.rejects("Silent eviction did not time out") { try budget().trim(to: 0) }
        manager.evictionSilent = false; manager.items = []
        try resetState()
        let folder = try item("folder", parent: "root", kind: "directory")
        let nested = try item("nested", size: 5, parent: "folder")
        let nestedDirectory = try item("nested-directory", parent: "folder", kind: "directory")
        let deep = try item("deep", size: 4, parent: "nested-directory")
        peer.mutate { $0.catalog = [folder, nested, nestedDirectory, deep] }
        let folderLease = try budget().reserve(WireItem.decode(folder), pin: true, operation: nil)!
        try expect(folderLease.count == 2, "Folder pin did not include recursive file descendants")
        budget().release(folderLease)
        peer.mutate { $0.catalog = [try! item("folder", parent: "folder", kind: "directory")] }
        try NativeTests.rejects("Folder catalog cycle was accepted") { _ = try budget().reserve(WireItem.decode(folder), operation: nil) }
        peer.mutate { $0.catalog = [try! item("huge", size: UInt64.max, parent: "folder"), try! item("more", size: 1, parent: "folder")] }
        try NativeTests.rejects("Folder size overflow was accepted") { _ = try budget().reserve(WireItem.decode(folder), operation: nil) }
        peer.mutate { $0.catalog = []; $0.limit = nil }
        _ = try budget().reserve(request, operation: nil)
        try resetState()
        let stateURL=peer.paths.group.appendingPathComponent("native-budget.json")
        try JSONSerialization.data(withJSONObject:["reservations":[:],"usage":["retired":["used":9,"pinned":0,"dirty":0]]]).write(to:stateURL)
        try FileManager.default.setAttributes([.posixPermissions:0o600],ofItemAtPath:stateURL.path)
        let registered=system.registered;system.registered=[]
        try budget().trim(to:1024)
        try expect(peer.inspect{$0.calls.contains{$0["native_confirmed_unregistered"] as? Bool==true}},"OS-confirmed removed domain did not clear its conservative accounting")
        system.registered=registered
        try resetState()
        manager.items=[BudgetMaterialized(try WireItem.decode(clean),current:false)]
        try budget().acknowledgeLocalContent(WireItem.decode(clean))
        try budget().trim(to:1024)
        try expect(try reservations().isEmpty,"Materialized delivered receipt was not reconciled")
        try expect(peer.inspect{$0.calls.contains{($0["native_cached_versions"] as? [String:String])?["clean"]=="v1"}},"Delivered receipt did not verify a non-current native observation")
        manager.items=[]
        let lockURL=peer.paths.group.appendingPathComponent("native-budget.lock")
        let lockFD=Darwin.open(lockURL.path,O_RDWR|O_CLOEXEC)
        try expect(lockFD>=0 && flock(lockFD,LOCK_EX|LOCK_NB)==0,"Could not acquire isolated contention fixture")
        let realClockBudget=NativeMaterializationBudget(bridge:own,domain:"budget-domain",system:system,timeout:0.005)
        try NativeTests.rejects("Contended cache lock did not time out") {try realClockBudget.renew([])}
        _=flock(lockFD,LOCK_UN);Darwin.close(lockFD)
        try resetState()
        let blocked=peer.paths.group.appendingPathComponent("unsearchable")
        try FileManager.default.createDirectory(at:blocked,withIntermediateDirectories:false,attributes:[.posixPermissions:0o700])
        let blockedFile=blocked.appendingPathComponent("file")
        try Data([1]).write(to:blockedFile)
        manager.items=[BudgetMaterialized(try WireItem.decode(item("denied")))];manager.urls["denied"]=blockedFile
        try FileManager.default.setAttributes([.posixPermissions:0o000],ofItemAtPath:blocked.path)
        try NativeTests.rejects("Uninspectable native resident was treated as zero bytes") {try budget().trim(to:1024)}
        try FileManager.default.setAttributes([.posixPermissions:0o700],ofItemAtPath:blocked.path)
        manager.items=[]
        let wide=try (0..<4000).map {try item("wide-"+String(repeating:"x",count:220)+String($0),size:1,parent:"folder")}
        peer.mutate{$0.catalog=wide;$0.limit=nil}
        try NativeTests.rejects("Oversized durable reservation plan was accepted") {_=try budget().reserve(WireItem.decode(folder),operation:nil)}
        peer.mutate{$0.catalog=[];$0.limit=1024}
        try resetState()
        try testWatcher(peer, manager, system, own)
        print("PASS native watcher pin scheduling, cooldown and anchor recovery")
        try testWatcherDomainIsolation(paths)
        print("PASS native watcher per-domain failure isolation and pending-request cancellation")
        try testWatcherPages(paths)
        print("PASS native bounded snapshot paging, stable anchors, healthy-domain fairness and unseen deletions")
        print("PASS mocked native cache accounting, owned leases, protected LRU, rollback, recursive pins, SDK error/timeouts and watcher reconciliation")
    }

    private static func testWatcher(_ peer: BudgetPeer, _ manager: BudgetManager, _ system: BudgetSystem, _ bridge: BackendBridge) throws {
        var now = Date()
        manager.items = []; manager.downloads = []; manager.signalled = []
        let raws = try (0..<6).map { try item("watch-\($0)", parent: "folder-\($0)", pinned: true) }
        peer.mutate { value in
            value.catalog = raws
            for raw in raws { value.items[raw["id"] as! String] = raw }
        }
        let watcher = NativeDomainWatcher(bridge: bridge, system: system, timeout: 0.02, now: { now })
        try expect(try poll(watcher) == nil, "Initial watcher snapshot failed")
        try expect(manager.downloads.count == 4 && manager.signalled.contains("working_set"), "Watcher failed to bound eager download scheduling")
        try expect(try poll(watcher) == nil && manager.downloads.count == 6, "Unscheduled pin descendants were lost or throttling failed")
        let count = manager.downloads.count
        try expect(try poll(watcher) == nil && manager.downloads.count == count, "Watcher repeated requests inside its cooldown")
        now.addTimeInterval(16)
        try expect(try poll(watcher) == nil && manager.downloads.count == count + 4, "Watcher did not retry after cooldown")
        peer.mutate { $0.expireChanges = true }
        try expect(try poll(watcher) == nil, "Expired anchor was not reset for a fresh snapshot")
        manager.signalError = budgetFailure
        try expect(try poll(watcher) == nil, "Native signal failure stopped catalog reconciliation")
        manager.signalError = nil
        peer.mutate { $0.malformedCatalog = true }
        try expect(try poll(watcher) != nil, "Incomplete catalog was acknowledged")
        peer.mutate { $0.malformedCatalog = false }
        system.managerValue = nil
        try expect(try poll(watcher) != nil, "Watcher ignored unavailable native manager")
        system.managerValue = manager; system.error = budgetFailure
        try expect(try poll(watcher) != nil, "Watcher ignored native domain enumeration failure")
        system.error = nil; system.silent = true
        try expect(try poll(watcher) != nil, "Watcher did not time out a silent domain request")
        system.silent = false
        watcher.stop()
        let calls = peer.inspect { $0.calls.count }
        try expect(try poll(watcher) == nil && peer.inspect { $0.calls.count } == calls, "Stopped watcher issued more requests")
    }
    private static func testWatcherDomainIsolation(_ paths: NativePaths) throws {
        let peer = try BudgetPeer(root: paths.group); defer { peer.stop() }
        try expect(try peer.listenerSuppressesSIGPIPE(), "Queued watcher cancellation requires SIGPIPE suppression on the listener itself")
        let healthy = BudgetManager(peer.paths.group)
        let system = BudgetSystem(nil)
        system.registered = ["broken", "healthy"].map {
            NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier($0), displayName: $0)
        }
        system.domainManagers["healthy"] = healthy
        peer.mutate {
            $0.configured = [["id": "broken", "smart": true], ["id": "healthy", "smart": true]]
            $0.catalogByDomain["healthy"] = [try! item("healthy-file", parent: "healthy-folder")]
            $0.malformedDomains = ["broken"]
        }
        let bridge = BackendBridge(paths: peer.paths)
        let watcher = NativeDomainWatcher(bridge: bridge, system: system, timeout: 0.1)
        defer { watcher.stop() }
        // A missing first manager must not prevent a later local catalog signal.
        try expect(try poll(watcher) != nil && healthy.signalled.contains("healthy-folder"),
                   "A missing domain manager hid a healthy later domain")
        system.domainManagers["broken"] = BudgetManager(peer.paths.group)
        healthy.signalled = []
        try expect(try poll(watcher) != nil && healthy.signalled.contains("healthy-folder"),
                   "A malformed first catalog hid a healthy later domain")
        peer.mutate { $0.malformedDomains = [] }
        try expect(try poll(watcher) == nil, "A recovered domain retained an old poll failure")
        healthy.signalled = []
        peer.mutate { $0.expireChanges = true }
        try expect(try poll(watcher) == nil && healthy.signalled.contains("healthy-folder"),
                   "An expired first-domain anchor hid a healthy later domain")

        let cancelled = NativeDomainWatcher(bridge: bridge, system: system, timeout: 0.1)
        peer.mutate { $0.calls = []; $0.blockedCatalogDomain = "broken" }
        let done = DispatchSemaphore(value: 0); var failure: Error?
        cancelled.poll { failure = $0; done.signal() }
        try wait(peer.catalogStarted)
        cancelled.stop()
        try wait(done)
        try expect((failure as NSError?)?.code == NSUserCancelledError,
                   "Stopping a domain poll did not preserve cancellation")
        try expect(!peer.inspect { $0.calls.contains { (["watcher_snapshot", "changes"].contains($0["op"] as? String ?? "")) && $0["domain"] as? String == "healthy" } },
                   "A cancelled poll continued into another domain")
    }

    private static func testWatcherPages(_ paths: NativePaths) throws {
        let peer = try BudgetPeer(root: paths.group); defer { peer.stop() }
        let manager = BudgetManager(peer.paths.group)
        let system = BudgetSystem(manager)
        let healthy = BudgetManager(peer.paths.group)
        system.registered.append(NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("healthy-page"), displayName: "Healthy"))
        system.domainManagers["healthy-page"] = healthy
        let bridge = BackendBridge(paths: peer.paths)
        peer.mutate {
            $0.catalog = try! (0..<300).map { try item(String(format: "paged-%03d", $0), parent: "parent-\($0)", pinned: $0 == 299) }
            $0.limit = nil
            $0.configured.append(["id": "healthy-page", "smart": true])
            $0.catalogByDomain["healthy-page"] = [try! item("healthy-file", parent: "healthy-folder")]
        }
        let watcher = NativeDomainWatcher(bridge: bridge, system: system, timeout: 0.1)
        defer { watcher.stop() }
        try expect(try poll(watcher) == nil, "First bounded watcher page failed")
        let first = peer.inspect { $0.calls.filter { $0["op"] as? String == "watcher_snapshot" && $0["domain"] as? String == "budget-domain" } }
        try expect(first.count == 1 && first[0]["id"] == nil && first[0]["anchor"] == nil,
                   "Initial watcher page was unbounded or carried a stale cursor")
        try expect(healthy.signalled.contains("healthy-folder"), "A large paged snapshot delayed the healthy later domain")
        try expect(manager.signalled.contains("root") && !manager.downloads.contains("paged-299"),
                   "Initial page did not signal root or consumed unseen pin metadata")
        try expect(try poll(watcher) == nil && manager.downloads.contains("paged-299"),
                   "Later snapshot page lost a nested pin")
        let second = peer.inspect { $0.calls.filter { $0["op"] as? String == "watcher_snapshot" && $0["domain"] as? String == "budget-domain" } }
        try expect(second.count == 2 && second[1]["id"] as? String == "paged-255" && second[1]["anchor"] as? String == "next",
                   "Watcher did not preserve its initial baseline across pages")
        try expect(try poll(watcher) == nil && peer.inspect { $0.calls.contains { $0["op"] as? String == "changes" && $0["anchor"] as? String == "next" } },
                   "Completed watcher snapshot did not catch up from its original baseline")

        manager.signalled = []
        peer.mutate { $0.catalog = [] }
        try expect(try poll(watcher) == nil && manager.signalled.contains("working_set"),
                   "An unseen deletion failed to notify the native working set")
        peer.mutate { $0.catalog = try! (0..<300).map { try item(String(format: "paged-%03d", $0)) } }
        let invalid = NativeDomainWatcher(bridge: bridge, system: system, timeout: 0.1)
        defer { invalid.stop() }
        peer.mutate { $0.snapshotReply = ["ok": true, "items": $0.catalog, "anchor": "one"] }
        try expect(try poll(invalid) != nil, "Oversized watcher page was accepted")
        peer.mutate { $0.snapshotReply = ["ok": true, "items": [], "anchor": "one", "next_id": ""] }
        try expect(try poll(invalid) != nil, "Empty watcher cursor was accepted")
        peer.mutate { $0.snapshotReply = ["ok": true, "items": [try! item("a")], "anchor": "one", "next_id": "a"] }
        try expect(try poll(invalid) == nil, "Valid first page was rejected")
        peer.mutate { $0.snapshotReply = ["ok": true, "items": [], "anchor": "two"] }
        try expect(try poll(invalid) != nil, "Changed watcher baseline was accepted")
        peer.mutate { $0.snapshotReply = ["ok": true, "items": [], "anchor": "one", "next_id": "a"] }
        try expect(try poll(invalid) != nil, "Nonadvancing watcher cursor was accepted")
        peer.mutate { $0.snapshotReply = ["ok": false, "error": ["code": "sync_anchor_expired"]] }
        try expect(try poll(invalid) == nil, "Expired partial snapshot did not reset its own cursor")
        peer.mutate { $0.snapshotReply = nil; $0.calls = []; $0.catalog = [] }
        try expect(try poll(invalid) == nil && peer.inspect { $0.calls.contains { $0["op"] as? String == "watcher_snapshot" && $0["id"] == nil && $0["anchor"] == nil } },
                   "Expired snapshot restarted from a partial cursor")
    }

}
