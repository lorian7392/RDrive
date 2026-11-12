import Foundation
import AppKit
import FileProvider
import FileProviderUI
import FinderSync

private final class UIEmptyEnumerator: NSObject, NSFileProviderEnumerator {
    func invalidate() {}
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        observer.finishEnumerating(upTo: nil)
    }
}

private final class UICoverageManager: NativeDomainManaging {
    var url: URL?
    var urlError: Error?
    var omitURLCompletion = false
    var downloads: [String] = []
    var evictions: [String] = []
    var signals: [String] = []
    var actionError: Error?
    var resolvedErrors: [Int] = []
    var omitResolvedCompletion = false
    init(url: URL) { self.url = url }
    func enumeratorForMaterializedItems() -> NSFileProviderEnumerator { UIEmptyEnumerator() }
    func getUserVisibleURL(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (URL?, Error?) -> Void) {
        if !omitURLCompletion { completionHandler(url, urlError) }
    }
    func evictItem(identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        evictions.append(identifier.rawValue); completionHandler(actionError)
    }
    func signalEnumerator(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        signals.append(identifier.rawValue); completionHandler(nil)
    }
    func signalErrorResolved(_ error: NSError, completionHandler: @escaping (Error?) -> Void) {
        resolvedErrors.append(error.code)
        if !omitResolvedCompletion { completionHandler(actionError) }
    }
    func requestDownloadForItem(withIdentifier identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) {
        downloads.append(identifier.rawValue); completionHandler(actionError)
    }
    func temporaryDirectoryURL() throws -> URL { url!.deletingLastPathComponent() }
}

private final class FinderSpy {
    var badges: [(String, URL)] = []
    var registrations: [String] = []
    var roots: [Set<URL>] = []
    var selected: [URL]?
    var target: URL?
    var native = true
    var manager: UICoverageManager?
    var state = "online"
    var dataless: Bool? = false
    var opened: [URL] = []
    var requests: [[String: Any]] = []
    var responseError: Error?
    var resolveReply: [String: Any] = ["domain": "ui-fixture", "id": "ui-file"]
    func integration() -> NativeFinderIntegration {
        var result = NativeFinderIntegration(
            registerBadge: { id, _, _ in self.registrations.append(id) },
            setRoots: { self.roots.append($0) }, selection: { self.selected }, target: { self.target },
            identify: { _, done in
                done(self.native ? NSFileProviderItemIdentifier("ui-file") : nil,
                     self.native ? NSFileProviderDomainIdentifier("ui-fixture") : nil, nil)
            }, manager: { _ in self.manager }, open: { self.opened.append($0) })
        result.setBadge = { self.badges.append(($0, $1)) }
        result.dataless = { _ in self.dataless }
        result.request = { _, request in
            self.requests.append(request)
            if let failure = self.responseError { throw failure }
            if request["op"] as? String == "resolve" { return self.resolveReply }
            if request["op"] as? String == "status" { return ["state": self.state] }
            return ["ok": true]
        }
        return result
    }
}

enum UICoverageTests {
    static func pump(_ message: String, until predicate: () -> Bool) throws {
        let deadline = Date(timeIntervalSinceNow: 3)
        while !predicate() && Date() < deadline { RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.01)) }
        try expect(predicate(), message)
    }
    static func reject(_ message: String, _ body: () throws -> Void) throws {
        do { try body() } catch { return }
        throw TestFailure(message: message)
    }
    static func run(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        let source = paths.group.appendingPathComponent("ui-file.txt")
        try Data("synthetic UI data".utf8).write(to: source)
        let spy = FinderSpy()
        spy.selected = [source]
        spy.manager = UICoverageManager(url: source)
        let roots = [WatchedRoot(path: paths.group.path, domain: "ui-fixture")]
        try JSONEncoder().encode(roots).write(to: paths.roots)
        let finder = RDriveFinderSync(bridge: bridge, integration: spy.integration(), startTimer: false)
        try expect(spy.registrations == ["online", "cached", "pinned", "syncing", "error"], "Finder badge definitions changed")
        try expect(spy.roots.last?.contains(paths.group.standardizedFileURL) == true, "Finder did not use explicit watched roots")
        let count = spy.roots.count
        finder.reloadRoots()
        try expect(spy.roots.count == count, "Identical monitored roots were needlessly replaced")
        try expect(finder.toolbarItemName == "RDrive" && !finder.toolbarItemToolTip.isEmpty && finder.toolbarItemImage.size.width > 0, "Finder toolbar metadata missing")
        let menu = finder.menu(for: .contextualMenuForItems)
        try expect(menu?.items.map { $0.title } == ["Keep Offline", "Allow Automatic Removal", "Download Now", "Remove Download", "Show Sync Status"], "Finder menu actions changed")
        try expect(menu?.items.allSatisfy { ($0.representedObject as? [URL]) == [source] } == true, "Finder menu selection was not retained")
        spy.selected = nil; spy.target = source
        try expect(finder.menu(for: .contextualMenuForContainer)?.numberOfItems == 5, "Finder target fallback missing")
        spy.target = nil
        try expect(finder.menu(for: .contextualMenuForItems) == nil, "Finder manufactured an empty selection")
        let resolved = DispatchSemaphore(value: 0)
        var nativeResult: (String, String, Bool)?
        finder.resolve(source) { nativeResult = try? $0.get(); resolved.signal() }
        try wait(resolved)
        try expect(nativeResult?.0 == "ui-fixture" && nativeResult?.1 == "ui-file" && nativeResult?.2 == true, "Native identity provenance was lost")
        for (state, dataless, expected) in [("pinned", true, "syncing"), ("cached", true, "online"), ("online", false, "cached"), ("queued", false, "syncing"), ("conflict", false, "error"), ("unknown", false, "error")] {
            spy.state = state; spy.dataless = dataless
            let previous = spy.badges.count
            finder.requestBadgeIdentifier(for: source)
            try pump("Finder badge callback missing") { spy.badges.count > previous }
            try expect(spy.badges.last?.0 == expected, "Finder materialization/state badge mismatch")
        }
        spy.dataless = nil; spy.state = "pinned"
        let previous = spy.badges.count
        finder.beginObservingDirectory(at: source)
        try pump("Unknown materialization badge missing") { spy.badges.count > previous }
        try expect(spy.badges.last?.0 == "pinned", "Unknown physical state overrode the policy")
        finder.endObservingDirectory(at: paths.group)
        try expect(RDriveFinderSync.isDatalessRegularFile(source) == false, "Ordinary synthetic file was labeled dataless")
        try expect(RDriveFinderSync.isDatalessRegularFile(paths.group) == nil, "Directory was accepted as dataless regular content")
        try expect(RDriveFinderSync.isDatalessRegularFile(paths.group.appendingPathComponent("missing-ui-file")) == nil, "Missing synthetic file was considered materialized")
        let action = NSMenuItem()
        action.representedObject = [source]
        finder.act(NSMenuItem(), action: "pin")
        for key in ["pin", "unpin"] {
            let previous = spy.requests.count
            finder.act(action, action: key)
            try pump("Finder policy request missing") { spy.requests.dropFirst(previous).contains { $0["op"] as? String == "pin" } }
            let request = spy.requests.dropFirst(previous).first { $0["op"] as? String == "pin" }!
            try expect(request["pinned"] as? Bool == (key == "pin"), "Finder policy value changed")
        }
        finder.act(action, action: "download")
        try pump("Fake native download was not scheduled") { spy.manager?.downloads == ["ui-file"] }
        finder.act(action, action: "evict")
        try pump("Fake native eviction was not scheduled") { spy.manager?.evictions == ["ui-file"] }
        let failure = NSError(domain: "SyntheticUI", code: 17, userInfo: [NSLocalizedDescriptionKey: "Synthetic action failure"])
        spy.manager?.actionError = failure
        let beforeError = spy.opened.count
        finder.act(action, action: "download")
        try pump("Finder failed-download message missing") { spy.opened.count > beforeError }
        try expect(spy.opened.last?.host == "message", "Finder opened the wrong companion route")
        let beforeEvictError = spy.opened.count
        finder.act(action, action: "evict")
        try pump("Finder failed-eviction message missing") { spy.opened.count > beforeEvictError }
        spy.manager = nil
        let beforeMissingManager = spy.opened.count
        finder.act(action, action: "download")
        try pump("Missing manager was not reported") { spy.opened.count > beforeMissingManager }
        let beforeStatus = spy.opened.count
        finder.act(action, action: "status")
        try pump("Finder status route missing") { spy.opened.count > beforeStatus }
        try expect(spy.opened.last?.host == "status", "Status route did not preserve provenance")
        spy.manager = UICoverageManager(url: source)
        let beforeSelectorStatus = spy.opened.count
        for item in menu!.items {
            guard let selector = item.action, let target = item.target as? NSObject else { throw TestFailure(message: "Menu action has no target/selector") }
            _ = target.perform(selector, with: item)
        }
        try pump("Actual menu selectors did not reach their injected actions") { spy.manager?.downloads.count == 1 && spy.manager?.evictions.count == 1 && spy.opened.count > beforeSelectorStatus }
        spy.native = false
        var fallbackDone = false
        finder.resolve(source) { nativeResult = try? $0.get(); fallbackDone = true }
        try pump("Mapped-path resolver callback missing") { fallbackDone }
        try expect(nativeResult?.2 == false, "Mapped path was incorrectly promoted to native File Provider")
        let beforeMappedPin = spy.opened.count
        finder.act(action, action: "pin")
        try pump("Mapped network path offline action was not refused") { spy.opened.count > beforeMappedPin }
        spy.resolveReply = [:]
        var invalidDone = false
        var failed = false
        finder.resolve(source) { if case .failure = $0 { failed = true }; invalidDone = true }
        try pump("Malformed resolve response did not complete") { invalidDone }
        try expect(failed, "Malformed mapped identity was accepted")
        let absent = RDriveFinderSync(bridge: nil, integration: spy.integration(), startTimer: false)
        var absentDone = false
        absent.resolve(source) { if case .failure = $0 { failed = true }; absentDone = true }
        try pump("Unavailable bridge resolver did not complete") { absentDone }
        try FileManager.default.removeItem(at: paths.roots)
        finder.reloadRoots()
        try expect(spy.roots.last?.isEmpty == true, "Invalid roots failed to clear explicit monitoring")
        do {
            let timerFinder = RDriveFinderSync(bridge: bridge, integration: spy.integration())
            timerFinder.beginObservingDirectory(at: source)
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 1.1))
        }
        try status(paths, bridge, source)
        try repairStatus(bridge)
        try expect(NSApp == nil || (!NSApp.isRunning && !NSApp.windows.contains(where: { $0.isVisible })), "Detached UI testing started an application loop or displayed a window")
        print("PASS mocked Finder menus/badges/actions/provenance and detached status UI; no Finder/OS manager/UI actions")
    }

    static func status(_ paths: NativePaths, _ bridge: BackendBridge, _ source: URL) throws {
        let domain = NSFileProviderDomainIdentifier("ui-fixture")
        let manager = UICoverageManager(url: source)
        var selectedDomain: NSFileProviderDomainIdentifier? = domain
        var readError: Error?
        var completed = 0
        var omitIdentity = false
        var identityError: Error?
        var resolvedDomain: NSFileProviderDomainIdentifier? = domain
        var resolvedItem: NSFileProviderItemIdentifier? = NSFileProviderItemIdentifier("canonical-file")
        var seen: [[String: Any]] = []
        let controller = RDriveStatusViewController(nibName: nil, bundle: nil)
        var integration = NativeStatusIntegration(makeBridge: { bridge }, readItem: { _, request, _ in
            if let error = readError { throw error }
            seen.append(request)
            return try NativeTests.fixtureItem(["name": "Status.txt", "smart": true, "sync_state": "cached", "sync_message": String(repeating: "s", count: 600)])
        }, domain: { _ in selectedDomain }, manager: { _ in manager }, identify: { _, done in
            if !omitIdentity { done(resolvedItem, resolvedDomain, identityError) }
        }, complete: { _ in completed += 1 })
        integration.timeout = 0.01
        let framed = try integration.request(bridge, ["op": "test.fragmented"], SocketOperation())
        try expect(framed["value"] as? String == "fragmented reply", "Status request boundary changed bridge framing")
        controller.integration = integration
        func details() -> String {
            let stack = controller.view.subviews.compactMap { $0 as? NSStackView }.first!
            return (stack.arrangedSubviews[1] as! NSTextField).stringValue
        }
        controller.prepare(forError: NSError(domain: "SyntheticUI", code: 1, userInfo: [NSLocalizedDescriptionKey: "Visible synthetic error"]))
        try expect(details() == "Visible synthetic error" && controller.preferredContentSize.width == 480, "Detached status error UI failed")
        controller.prepare(forAction: "unknown", itemIdentifiers: [])
        try expect(details().hasPrefix("Select RDrive files"), "Invalid status action was not rejected")
        selectedDomain = nil
        controller.prepare(forAction: "rdrive.status", itemIdentifiers: [providerID("one")])
        try expect(details().hasPrefix("Select RDrive files"), "Missing native domain was accepted")
        selectedDomain = domain
        controller.prepare(forAction: "rdrive.status", itemIdentifiers: Array(repeating: providerID("one"), count: 129))
        try expect(details().hasPrefix("Select RDrive files"), "Excessive status selection was accepted")
        controller.prepare(forAction: "rdrive.status", itemIdentifiers: (0..<12).map { providerID("file-" + String($0)) })
        try pump("Status request did not finish") { details().contains("And 2 more selected files.") }
        try expect(seen.count == 10 && details().contains("Available offline · Smart Synchronization"), "Status item bound/Smart labels changed")
        try expect(!details().contains(String(repeating: "s", count: 501)), "Remote status message was not bounded")
        let ordinary = providerID("ordinary-file")
        try expect(try controller.canonicalIdentifier(ordinary, domain: domain) == ordinary, "Stable item identity was unnecessarily resolved")
        let privateID = providerID("__fp/fs/synthetic")
        try expect(try controller.canonicalIdentifier(privateID, domain: domain).rawValue == "canonical-file", "OS document identity was not canonicalized")
        resolvedDomain = NSFileProviderDomainIdentifier("different-domain")
        try reject("Cross-domain identity accepted") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        resolvedDomain = domain; resolvedItem = privateID
        try reject("Unresolved OS private identity accepted") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        resolvedItem = nil
        try reject("Missing canonical identity accepted") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        resolvedItem = providerID("canonical-file")
        identityError = NSError(domain: "SyntheticUI", code: 2)
        try reject("Identity failure ignored") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        identityError = nil; omitIdentity = true
        try reject("Identity timeout ignored") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        omitIdentity = false; manager.omitURLCompletion = true
        try reject("File URL timeout ignored") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        manager.omitURLCompletion = false; manager.urlError = NSError(domain: "SyntheticUI", code: 3)
        try reject("File URL failure ignored") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        manager.urlError = nil; manager.url = nil
        try reject("Missing user-visible file accepted") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        manager.url = source
        controller.integration.manager = { _ in nil }
        try reject("Unavailable status manager accepted") { _ = try controller.canonicalIdentifier(privateID, domain: domain) }
        controller.integration.manager = { _ in manager }
        readError = NSError(domain: "SyntheticUI", code: 4, userInfo: [NSLocalizedDescriptionKey: "Synthetic read failed"])
        controller.prepare(forAction: "rdrive.status", itemIdentifiers: [ordinary])
        try pump("Status read error did not finish") { details().contains("Status unavailable: Synthetic read failed") }
        controller.finish()
        try expect(completed == 1, "Status completion was not delegated exactly once")
    }

    static func repairStatus(_ bridge: BackendBridge) throws {
        let controller = RDriveStatusViewController(nibName: nil, bundle: nil)
        let domain = NSFileProviderDomainIdentifier("repair-domain")
        let exact = "version_conflict: item changed since the system snapshot"
        var itemState = "conflict"
        var itemMessage = exact
        var eligibility: [String: Any] = ["eligible": true, "mutation_id": "immutable-request",
                                        "message": "Clear this stale folder request and queue a folder refresh. Local contents and other pending edits are retained."]
        var response: [String: Any] = ["repaired": true, "refresh_queued": true]
        var repairError: Error?
        var requests: [[String: Any]] = []
        let notificationManager = UICoverageManager(url: URL(fileURLWithPath: "/private/tmp"))
        var integration = NativeStatusIntegration(makeBridge: { bridge }, readItem: { _, _, _ in
            try NativeTests.fixtureItem(["id": "folder", "kind": "directory", "smart": true,
                "sync_state": itemState, "sync_message": itemMessage])
        }, domain: { _ in domain }, manager: { _ in notificationManager }, identify: { _, _ in },
            complete: { _ in })
        integration.request = { _, request, _ in
            requests.append(request)
            try expect(request["domain"] as? String == domain.rawValue && request["id"] as? String == "folder",
                       "Repair UI lost its selected item identity")
            if request["op"] as? String == "smart_repair_status" { return eligibility }
            try expect(request["op"] as? String == "repair_stale_directory"
                && request["mutation_id"] as? String == "immutable-request" && request.count == 4,
                       "Repair action changed the preflight mutation or added a remote path")
            if let error = repairError { throw error }
            return response
        }
        controller.integration = integration
        func stack() -> NSStackView { controller.view.subviews.compactMap { $0 as? NSStackView }.first! }
        func details() -> String { (stack().arrangedSubviews[1] as! NSTextField).stringValue }
        func repairButton() -> NSButton { stack().arrangedSubviews[2] as! NSButton }
        func prepare(_ action: String = "rdrive.repair") throws {
            controller.prepare(forAction: action, itemIdentifiers: [providerID("folder")])
            try pump("Repair eligibility did not finish") { !repairButton().isHidden }
        }
        controller.repairConflict()
        try expect(requests.isEmpty, "An unselected repair sent a request")
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("one"), providerID("two")])
        try expect(details().contains("Select one Smart folder"), "Repair accepted multiple selected folders")
        try prepare()
        try expect(details().contains("Local contents and other pending edits are retained.") && repairButton().isEnabled,
                   "Eligible repair omitted its data-retention explanation")
        controller.repairConflict()
        try pump("Repair completion was not shown") { details().contains("Stale folder request cleared") }
        try expect(repairButton().isHidden && details().contains("refresh is queued"), "Local repair claimed a completed remote refresh")
        try expect(notificationManager.resolvedErrors == [NSFileProviderError.cannotSynchronize.rawValue, NSFileProviderError.serverUnreachable.rawValue],
                   "The confirmed repair did not ask Finder to retry its cached error")
        let completed = requests.count
        controller.repairConflict()
        try expect(requests.count == completed, "Completed repair was sent twice")

        notificationManager.actionError = NSError(domain: "SyntheticNotification", code: 1, userInfo: [NSLocalizedDescriptionKey: "Retry notification unavailable"])
        try prepare()
        controller.repairConflict()
        try pump("A failed Finder notification hid the saved repair") { details().contains("notification failed: Retry notification unavailable") }
        try expect(details().contains("Stale folder request cleared"), "A notification failure was reported as a lost repair")
        notificationManager.actionError = nil
        notificationManager.omitResolvedCompletion = true
        integration.timeout = 0.001
        controller.integration = integration
        try prepare()
        controller.repairConflict()
        try pump("An unanswered Finder notification hid the saved repair") { details().contains("notification timed out") }
        notificationManager.omitResolvedCompletion = false
        integration.timeout = 20
        var unavailableManager = integration
        unavailableManager.manager = { _ in nil }
        controller.integration = unavailableManager
        try prepare()
        controller.repairConflict()
        try pump("An unavailable native manager hid the saved repair") { details().contains("Reopen this connection") }
        controller.integration = integration

        itemState = "error"
        itemMessage = "unsupported: fetch this file before editing metadata"
        try prepare("rdrive.status")
        try expect(details().contains(itemMessage) && requests.last?["op"] as? String == "smart_repair_status",
                   "A failed folder no-op did not ask the backend whether it is safely repairable")
        controller.repairConflict()
        try pump("The failed folder no-op repair did not finish") { details().contains("Stale folder request cleared") }
        eligibility = ["eligible": false, "message": "Real folder metadata changes require review."]
        let failedRequests = requests.count
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("folder")])
        try pump("An ineligible failed folder omitted its protected-intent reason") { details().contains("Real folder metadata changes require review.") }
        try expect(repairButton().isHidden && requests.count == failedRequests + 1,
                   "A failed folder was repaired without backend approval")
        controller.repairConflict()
        try expect(requests.count == failedRequests + 1, "An ineligible failed folder sent a repair")
        eligibility = ["eligible": true, "mutation_id": "immutable-request",
                       "message": "Clear this stale folder request and queue a folder refresh. Local contents and other pending edits are retained."]

        itemMessage = "excluded_by_filter: synchronization filters exclude Backup/a.part; built-in temporary rule *.part"
        eligibility["filter_reason"] = "Excluded by temporary-file rule *.part at Backup/a.part"
        for state in ["blocked", "queued"] {
            itemState = state
            try prepare("rdrive.status")
            try expect(details().contains("synchronization filters exclude Backup/a.part"),
                       "Filter-blocked repair omitted its reason")
            try expect(details().contains("Excluded by temporary-file rule *.part"), "The matched rule was omitted")
            try expect(repairButton().title == "Repair Sync Issue", "The filter repair used a conflict-only label")
        }
        itemState = "queued"
        itemMessage = "server_unreachable: waiting for a connection"
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("folder")])
        try pump("An ordinary queued folder exposed a stale repair") { details().contains("no eligible stale Smart folder request") }
        try expect(repairButton().isHidden, "Ordinary queued work exposed a repair button")
        itemState = "conflict"
        itemMessage = exact
        eligibility.removeValue(forKey: "filter_reason")

        try prepare("rdrive.status")
        response = ["repaired": false]
        controller.repairConflict()
        try pump("Missing repair acknowledgment was ignored") { details().contains("Repair stopped:") && details().contains("did not confirm") }
        response = ["repaired": true, "refresh_queued": true]
        try prepare()
        repairError = NSError(domain: "SyntheticRepair", code: 1, userInfo: [NSLocalizedDescriptionKey: "A newer edit needs review"])
        controller.repairConflict()
        try pump("Repair failure was not shown") { details().contains("Repair stopped: A newer edit needs review") }
        repairError = nil

        eligibility = ["eligible": false, "message": "Local metadata changes require review in the RDrive command-line client."]
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("folder")])
        try pump("Protected local intent did not show a reason") { details().contains("Local metadata changes require review") }
        try expect(repairButton().isHidden, "Protected local intent exposed a repair button")
        eligibility = ["eligible": true]
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("folder")])
        try pump("Missing immutable repair identity was accepted") { details().contains("Status unavailable:") }
        eligibility = ["eligible": false]
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("folder")])
        try pump("Missing eligibility reason did not show review guidance") { details().contains("requires review in the RDrive command-line client") }
        itemState = "online"
        controller.prepare(forAction: "rdrive.repair", itemIdentifiers: [providerID("folder")])
        try pump("An already-cleared conflict had no guidance") { details().contains("no eligible stale Smart folder request") }
        try expect(repairButton().isHidden, "An already-cleared conflict exposed a repair button")
        itemState = "conflict"
        eligibility = ["eligible": true, "mutation_id": "immutable-request", "message": "Safe local repair"]
        try prepare()
        controller.prepare(forError: NSError(domain: "SyntheticRepair", code: 2))
        try expect(repairButton().isHidden, "Error context retained an earlier repair action")
        controller.finish()
        for failed in [false, true] {
            let started = DispatchSemaphore(value: 0)
            let release = DispatchSemaphore(value: 0)
            var delayed = integration
            delayed.readItem = { _, _, _ in
                started.signal()
                _ = release.wait(timeout: .now() + 3)
                if failed { throw NativeFailure.bridge("Cancelled old status") }
                return try NativeTests.fixtureItem(["kind": "file"])
            }
            controller.integration = delayed
            controller.prepare(forAction: "rdrive.status", itemIdentifiers: [providerID("folder")])
            try wait(started)
            controller.prepare(forAction: "unknown", itemIdentifiers: [])
            release.signal()
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.03))
            try expect(details().hasPrefix("Select RDrive files"), "An old status callback replaced the current selection")
        }
        for failed in [false, true] {
            let started = DispatchSemaphore(value: 0)
            let release = DispatchSemaphore(value: 0)
            controller.integration = integration
            try prepare()
            var delayed = integration
            delayed.request = { _, request, _ in
                try expect(request["op"] as? String == "repair_stale_directory", "Unexpected delayed repair request")
                started.signal()
                _ = release.wait(timeout: .now() + 3)
                if failed { throw NativeFailure.bridge("Cancelled old repair") }
                return ["repaired": true, "refresh_queued": true]
            }
            controller.integration = delayed
            controller.repairConflict()
            try wait(started)
            controller.prepare(forError: NSError(domain: "SyntheticRepair", code: 3, userInfo: [NSLocalizedDescriptionKey: "Current selection"]))
            release.signal()
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.03))
            try expect(details() == "Current selection", "An old repair callback replaced the current selection")
        }
    }
}
