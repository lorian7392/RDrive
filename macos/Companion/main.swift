import Foundation
import AppKit
import FinderSync
import FileProvider
import Darwin

func printJSON(_ value: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), let string = String(data: data, encoding: .utf8) { print(string) }
}

func option(_ name: String, arguments: [String] = CommandLine.arguments) -> String? {
    guard let index = arguments.firstIndex(of: name), index + 1 < arguments.count else { return nil }
    return arguments[index + 1]
}

func configuredPaths(arguments: [String] = CommandLine.arguments, bundle: Bundle = .main) throws -> NativePaths {
    if let path = option("--group-directory", arguments: arguments) {
        let url = URL(fileURLWithPath: path, isDirectory: true).standardizedFileURL
        guard path.hasPrefix("/"), FileManager.default.fileExists(atPath: url.path) else {
            throw NativeFailure.invalid("The explicit development group directory must already exist.")
        }
        let attrs = try FileManager.default.attributesOfItem(atPath: url.path)
        guard attrs[.ownerAccountID] as? UInt32 == getuid(), (attrs[.posixPermissions] as? UInt16 ?? 0) & 0o077 == 0 else {
            throw NativeFailure.invalid("The development group directory must belong to this user with private permissions.")
        }
        return NativePaths(group: url, development: true)
    }
    if let development = bundle.object(forInfoDictionaryKey: "RDriveDevelopmentIPCRoot") as? String,
       !FileManager.default.fileExists(atPath: development) {
        try FileManager.default.createDirectory(atPath: development, withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700])
    }
    return try NativePaths.configured(bundle: bundle)
}

func runUntil(timeout: TimeInterval = 30, _ invoke: (@escaping (Error?) -> Void) -> Void) -> Int32 {
    let lock = NSLock()
    var finished = false
    var failure: Error?
    invoke { error in lock.lock(); failure = error; finished = true; lock.unlock() }
    let deadline = Date(timeIntervalSinceNow: timeout)
    while true {
        lock.lock(); let done = finished; lock.unlock()
        if done { break }
        if Date() >= deadline {
            printJSON(["ok": false, "error": ["domain": NSCocoaErrorDomain, "code": NSXPCConnectionInterrupted, "message": "The macOS extension service did not respond within 30 seconds."]])
            return 1
        }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05))
    }
    if let error = failure {
        let ns = error as NSError
        printJSON(["ok": false, "error": ["domain": ns.domain, "code": ns.code, "message": ns.localizedDescription]])
        return 1
    }
    return 0
}

/// The control parser uses the same OS defaults in production and narrow
/// boundaries in host tests; no command is acknowledged before its callback.
struct CompanionControlIntegration {
    var paths: ([String]) throws -> NativePaths
    var system: NativeSystemManaging
    var addDomain: (NSFileProviderDomain, @escaping (Error?) -> Void) -> Void
    var removeDomain: (NSFileProviderDomain, @escaping (URL?, Error?) -> Void) -> Void
    var identify: (URL, @escaping (NSFileProviderItemIdentifier?, NSFileProviderDomainIdentifier?, Error?) -> Void) -> Void
    var finderEnabled: () -> Bool
    var enableFinder: () -> Void
    var setTags: (URL, [String]) throws -> Void
    var budget: (BackendBridge, String) -> NativeMaterializationBudget
    var watch: (BackendBridge) -> Int32

    static var live: CompanionControlIntegration {
        CompanionControlIntegration(
            paths: { try configuredPaths(arguments: $0) }, system: NativeSystemManager(),
            addDomain: { NSFileProviderManager.add($0, completionHandler: $1) },
            removeDomain: { NSFileProviderManager.remove($0, mode: .preserveDownloadedUserData, completionHandler: $1) },
            identify: { NSFileProviderManager.getIdentifierForUserVisibleFile(at: $0, completionHandler: $1) },
            finderEnabled: { FIFinderSyncController.isExtensionEnabled },
            enableFinder: { FIFinderSyncController.showExtensionManagementInterface() },
            setTags: { url, names in
                var coordinationError: NSError?
                var failure: Error?
                NSFileCoordinator().coordinate(writingItemAt: url, options: .contentIndependentMetadataOnly, error: &coordinationError) { coordinated in
                    do { try (coordinated as NSURL).setResourceValue(names, forKey: .tagNamesKey) }
                    catch { failure = error }
                }
                if let error = failure ?? coordinationError { throw error }
            },
            budget: { NativeMaterializationBudget(bridge: $0, domain: $1) },
            watch: { watchDomains($0) })
    }
}

struct CompanionWatchIntegration {
    var signals: (@escaping () -> Void) -> [DispatchSourceSignal]
    var timer: (@escaping () -> Void) -> Timer
    var wait: () -> Void
    static var live: CompanionWatchIntegration {
        CompanionWatchIntegration(signals: { stop in
            var sources: [DispatchSourceSignal] = []
            for number in [SIGINT, SIGTERM] {
                Darwin.signal(number, SIG_IGN)
                let source = DispatchSource.makeSignalSource(signal: number, queue: .main)
                source.setEventHandler(handler: stop)
                source.resume(); sources.append(source)
            }
            return sources
        }, timer: { tick in Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in tick() } },
        wait: { RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.1)) })
    }
}

func watchDomains(_ bridge: BackendBridge, system: NativeSystemManaging = NativeSystemManager(), integration: CompanionWatchIntegration = .live) -> Int32 {
    let watcher = NativeDomainWatcher(bridge: bridge, system: system)
    var running = true
    let sources = integration.signals { running = false }
    let timer = integration.timer { watcher.poll() }
    watcher.poll()
    printJSON(["ok": true, "event": "native_watcher_started"])
    while running { integration.wait() }
    timer.invalidate(); sources.forEach { $0.cancel() }; watcher.stop()
    printJSON(["ok": true, "event": "native_watcher_stopped"])
    return 0
}

func controlCommand(arguments: [String] = CommandLine.arguments, integration: CompanionControlIntegration = .live) -> Int32? {
    if arguments.contains("--retry-sync") {
        guard let identifier = option("--retry-sync", arguments: arguments), !identifier.isEmpty, !identifier.hasPrefix("--") else {
            printJSON(["ok": false, "error": "A native domain identifier is required."]); return 2
        }
        return runUntil { done in
            integration.system.domains { domains, error in
                if let error = error { done(error); return }
                guard domains.contains(where: { $0.identifier.rawValue == identifier }) else {
                    done(NativeFailure.invalid("The native domain is not registered.")); return
                }
                let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
                guard let manager = integration.system.manager(for: domain) else {
                    done(NativeFailure.invalid("The domain manager is unavailable.")); return
                }
                signalNativeSyncErrorsResolved(manager, codes: [.serverUnreachable, .cannotSynchronize]) { error in
                    if error == nil { printJSON(["ok": true, "action": "--retry-sync", "domain": identifier,
                        "rescheduled_errors": ["server_unreachable", "cannot_synchronize"]]) }
                    done(error)
                }
            }
        }
    }
    if arguments.contains("--watch-domains") {
        do {
            let bridge = BackendBridge(paths: try integration.paths(arguments))
            _ = try bridge.request(["op": "domains"])
            return integration.watch(bridge)
        } catch { printJSON(["ok": false, "error": String(describing: error)]); return 1 }
    }
    if arguments.contains("--diagnose") {
        var report: [String: Any] = ["ok": true, "bundle_id": Bundle.main.bundleIdentifier ?? "", "finder_enabled": integration.finderEnabled()]
        do { report["group_directory"] = try integration.paths(arguments).group.path }
        catch { report["group_error"] = String(describing: error) }
        printJSON(report)
        return 0
    }
    if arguments.contains("--list-domains") {
        return runUntil { done in
            integration.system.domains { domains, error in
                if error == nil { printJSON(["ok": true, "domains": domains.map { ["id": $0.identifier.rawValue, "display_name": $0.displayName] }]) }
                done(error)
            }
        }
    }
    if let limitString = option("--trim-cache", arguments: arguments) {
        guard let limit = UInt64(limitString) else { printJSON(["ok": false, "error": "Pass a nonnegative cache byte limit."]); return 2 }
        return runUntil { done in
            DispatchQueue.global(qos: .utility).async {
                do {
                    let bridge = try BackendBridge(paths: integration.paths(arguments))
                    try integration.budget(bridge, "").trim(to: limit)
                    printJSON(["ok": true, "trimmed_to_max_total_bytes": limit]); done(nil)
                } catch { done(error) }
            }
        }
    }
    for action in ["--pin", "--unpin", "--check-cache"] {
        if let identifier = option(action, arguments: arguments) {
            guard let index = arguments.firstIndex(of: action), index + 2 < arguments.count else {
                printJSON(["ok": false, "error": "An item identifier is required."]); return 2
            }
            let id = arguments[index + 2]
            return runUntil { done in
                DispatchQueue.global(qos: .utility).async {
                    var reservation: [String]?
                    var budget: NativeMaterializationBudget?
                    do {
                        let bridge = BackendBridge(paths: try integration.paths(arguments))
                        let activeBudget = integration.budget(bridge, identifier); budget = activeBudget
                        if action != "--unpin" {
                            let item = try bridge.item(["op": "item", "domain": identifier, "id": id])
                            reservation = try activeBudget.reserve(item, pin: action == "--pin", operation: nil)
                        }
                        if action == "--check-cache" { activeBudget.release(reservation); printJSON(["ok": true, "cache_preflight": true]); done(nil); return }
                        _ = try bridge.request(["op": "pin", "domain": identifier, "id": id, "pinned": action == "--pin"])
                        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
                        integration.system.manager(for: domain)?.signalEnumerator(for: .workingSet) { _ in }
                        printJSON(["ok": true, "action": action, "domain": identifier, "id": id]); done(nil)
                    } catch { budget?.release(reservation); done(error) }
                }
            }
        }
    }
    if let identifier = option("--register-domain", arguments: arguments) {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: option("--display-name", arguments: arguments) ?? identifier)
        return runUntil { done in
            integration.addDomain(domain) { error in
                if error == nil { printJSON(["ok": true, "registered_domain": identifier]) }
                done(error)
            }
        }
    }
    if let identifier = option("--remove-domain", arguments: arguments) {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
        return runUntil { done in
            integration.removeDomain(domain) { preserved, error in
                if error == nil { printJSON(["ok": true, "removed_domain": identifier, "preserved_location": preserved?.path ?? ""]) }
                done(error)
            }
        }
    }
    if let identifier = option("--domain-root", arguments: arguments) {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
        return runUntil { done in
            guard let manager = integration.system.manager(for: domain) else { done(NativeFailure.invalid("The domain manager is unavailable.")); return }
            manager.getUserVisibleURL(for: .rootContainer) { url, error in
                if error == nil { printJSON(["ok": true, "path": url?.path ?? ""]) }
                done(error)
            }
        }
    }
    if let path = option("--resolve-file", arguments: arguments) {
        return runUntil { done in
            integration.identify(URL(fileURLWithPath: path)) { item, domain, error in
                if error == nil { printJSON(["ok": true, "id": item.map(wireID) ?? "", "domain": domain?.rawValue ?? ""]) }
                done(error)
            }
        }
    }
    if let path = option("--set-tags", arguments: arguments), let index = arguments.firstIndex(of: "--set-tags") {
        let names = Array(arguments.dropFirst(index + 2))
        do { try integration.setTags(URL(fileURLWithPath: path), names) }
        catch { printJSON(["ok": false, "error": (error as NSError).localizedDescription]); return 1 }
        printJSON(["ok": true, "tags": names]); return 0
    }
    for action in ["--signal", "--download", "--evict"] {
        if let identifier = option(action, arguments: arguments) {
            let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
            return runUntil { done in
                guard let manager = integration.system.manager(for: domain) else { done(NativeFailure.invalid("The domain manager is unavailable.")); return }
                let complete: (Error?) -> Void = { error in
                    if error == nil { printJSON(["ok": true, "action": action, "domain": identifier]) }
                    done(error)
                }
                if action == "--signal" { manager.signalEnumerator(for: .workingSet, completionHandler: complete); return }
                guard let index = arguments.firstIndex(of: action), index + 2 < arguments.count else {
                    done(NativeFailure.invalid("An item identifier is required.")); return
                }
                let item = providerID(arguments[index + 2])
                if action == "--download" { manager.requestDownloadForItem(withIdentifier: item, completionHandler: complete) }
                else { manager.evictItem(identifier: item, completionHandler: complete) }
            }
        }
    }
    if arguments.contains("--enable-finder") {
        integration.enableFinder()
        printJSON(["ok": true, "opened_extension_settings": true])
        return 0
    }
    let allowedGUI = Set(["--group-directory", "--show-status", "--show-message"])
    if let unknown = arguments.dropFirst().first(where: { $0.hasPrefix("--") && !allowedGUI.contains($0) }) {
        printJSON(["ok": false, "error": "Unknown native control option: " + unknown]); return 2
    }
    return nil
}

protocol CompanionProcessManaging: AnyObject {
    var isRunning: Bool { get }
    var processIdentifier: Int32 { get }
    func launch(executable: URL?, arguments: [String]) throws
}
final class NativeCompanionProcess: CompanionProcessManaging {
    private let process = Process()
    var isRunning: Bool { process.isRunning }
    var processIdentifier: Int32 { process.processIdentifier }
    func launch(executable: URL?, arguments: [String]) throws {
        process.executableURL = executable
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
    }
}

struct CompanionHostIntegration {
    var arguments: [String]
    var paths: () throws -> NativePaths
    var bridge: (NativePaths) -> BackendBridge
    var watcher: (BackendBridge) -> NativeDomainWatcher
    var request: (BackendBridge, [String: Any]) throws -> [String: Any]
    var configureStatus: (NSMenu) -> NSStatusItem?
    var process: () -> CompanionProcessManaging
    var serviceURL: () -> URL?
    var home: () -> String?
    var interrupt: (Int32) -> Void
    var timer: (TimeInterval, @escaping () -> Void) -> Timer
    var after: (@escaping () -> Void) -> Void
    var addDomain: (NSFileProviderDomain, @escaping (Error?) -> Void) -> Void
    var message: (String) -> Void
    var enableFinder: () -> Void
    var openStorage: () -> Void
    var quit: () -> Void

    static var live: CompanionHostIntegration {
        CompanionHostIntegration(arguments: CommandLine.arguments,
            paths: { try configuredPaths() }, bridge: { BackendBridge(paths: $0) },
            watcher: { NativeDomainWatcher(bridge: $0) }, request: { try $0.request($1) },
            configureStatus: { menu in
                NSApp.setActivationPolicy(.accessory)
                let status = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
                status.button?.image = NSImage(systemSymbolName: "externaldrive.connected.to.line.below", accessibilityDescription: "RDrive")
                status.menu = menu
                return status
            }, process: { NativeCompanionProcess() },
            serviceURL: { Bundle.main.resourceURL?.appendingPathComponent("rdrive") },
            home: { ProcessInfo.processInfo.environment["RDRIVE_HOME"] },
            interrupt: { _ = Darwin.kill($0, SIGINT) },
            timer: { interval, tick in Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { _ in tick() } },
            after: { DispatchQueue.main.asyncAfter(deadline: .now() + 1, execute: $0) },
            addDomain: { NSFileProviderManager.add($0, completionHandler: $1) },
            message: { text in
                let alert = NSAlert(); alert.messageText = "RDrive"; alert.informativeText = text
                alert.addButton(withTitle: "OK")
                NSApp.activate(ignoringOtherApps: true)
                alert.runModal()
            }, enableFinder: { FIFinderSyncController.showExtensionManagementInterface() },
            openStorage: { NSWorkspace.shared.open(FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/CloudStorage")) },
            quit: { NSApp.terminate(nil) })
    }
}

final class CompanionDelegate: NSObject, NSApplicationDelegate {
    private var status: NSStatusItem?
    private var service: CompanionProcessManaging?
    private let serviceLock = NSLock()
    private var terminating = false
    private var bridge: BackendBridge?
    private var refresh: Timer?
    private var catalogRefresh: Timer?
    private var watcher: NativeDomainWatcher?
    let menu = NSMenu()
    let integration: CompanionHostIntegration
    init(integration: CompanionHostIntegration = .live) { self.integration = integration; super.init() }

    func applicationDidFinishLaunching(_ notification: Notification) {
        status = integration.configureStatus(menu)
        do {
            let paths = try integration.paths()
            try paths.prepare()
            bridge = integration.bridge(paths)
            watcher = integration.watcher(bridge!)
            startService(paths)
            rebuildMenu()
            refresh = integration.timer(15) { [weak self] in self?.rebuildMenu() }
            catalogRefresh = integration.timer(3) { [weak self] in self?.watcher?.poll() }
        } catch {
            menu.addItem(withTitle: "Connection unavailable", action: nil, keyEquivalent: "")
            menu.addItem(withTitle: "Quit RDrive", action: #selector(quit), keyEquivalent: "q").target = self
            showMessage((error as NSError).localizedDescription)
        }
        if let domain = option("--show-status", arguments: integration.arguments),
           let index = integration.arguments.firstIndex(of: "--show-status"), index + 2 < integration.arguments.count {
            showStatus(domain: domain, id: integration.arguments[index + 2])
        }
        if let message = option("--show-message", arguments: integration.arguments) { showMessage(message) }
    }

    func applicationWillTerminate(_ notification: Notification) {
        refresh?.invalidate()
        catalogRefresh?.invalidate()
        watcher?.stop()
        serviceLock.lock(); terminating = true; let owned = service; serviceLock.unlock()
        if let owned = owned, owned.isRunning { integration.interrupt(owned.processIdentifier) }
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            guard url.scheme == "rdrive-native", let components = URLComponents(url: url, resolvingAgainstBaseURL: false) else { continue }
            let values = Dictionary((components.queryItems ?? []).map { ($0.name, $0.value ?? "") }, uniquingKeysWith: { _, last in last })
            if url.host == "status", let domain = values["domain"], let id = values["id"] { showStatus(domain: domain, id: id) }
            else if url.host == "message", let message = values["text"] { showMessage(message) }
        }
    }

    func startService(_ paths: NativePaths) {
        DispatchQueue.global(qos: .utility).async {
            if let bridge = self.bridge, (try? self.integration.request(bridge, ["op": "domains"])) != nil { return }
            let process = self.integration.process()
            var arguments = ["provider", "serve", "--socket", paths.socket.path, "--staging", paths.transfers.path]
            if paths.development { arguments += ["--development-tcp"] }
            if let home = self.integration.home() { arguments += ["--home", home] }
            do {
                self.serviceLock.lock(); defer { self.serviceLock.unlock() }
                if self.terminating { return }
                try process.launch(executable: self.integration.serviceURL(), arguments: arguments); self.service = process
            }
            catch { DispatchQueue.main.async { self.showMessage("RDrive could not start its connection service.") } }
            self.integration.after { self.rebuildMenu() }
        }
    }

    func rebuildMenu() {
        menu.removeAllItems()
        menu.addItem(withTitle: "RDrive Connections", action: nil, keyEquivalent: "")
        let connections = NSMenu(title: "Add Connection to Finder")
        connections.addItem(withTitle: "Loading connections…", action: nil, keyEquivalent: "")
        let parent = NSMenuItem(title: "Add Connection to Finder", action: nil, keyEquivalent: "")
        parent.submenu = connections; menu.addItem(parent)
        let refresh = NSMenu(title: "Refresh Connection Index")
        refresh.addItem(withTitle: "Loading connections…", action: nil, keyEquivalent: "")
        let refreshParent = NSMenuItem(title: "Refresh Connection Index", action: nil, keyEquivalent: "")
        refreshParent.submenu = refresh
        menu.addItem(refreshParent)
        DispatchQueue.global(qos: .utility).async {
            let reply = self.bridge.flatMap { try? self.integration.request($0, ["op": "domains"]) }
            DispatchQueue.main.async {
                connections.removeAllItems()
                refresh.removeAllItems()
                if let domains = reply?["domains"] as? [[String: Any]] {
                    for domain in domains {
                        guard let id = domain["id"] as? String else { continue }
                        let item = NSMenuItem(title: domain["display_name"] as? String ?? id, action: #selector(self.addDomain(_:)), keyEquivalent: "")
                        item.target = self; item.representedObject = domain; connections.addItem(item)
                        if domain["smart"] as? Bool == true {
                            let entry = refresh.addItem(withTitle: item.title, action: #selector(self.refreshConnection(_:)), keyEquivalent: "")
                            entry.target = self
                            entry.representedObject = id
                        }
                    }
                    if domains.isEmpty { connections.addItem(withTitle: "No connections configured", action: nil, keyEquivalent: "") }
                    if refresh.items.isEmpty { refresh.addItem(withTitle: "No Smart connections configured", action: nil, keyEquivalent: "") }
                } else {
                    connections.addItem(withTitle: "Connection service unavailable", action: nil, keyEquivalent: "")
                    refresh.addItem(withTitle: "Connection service unavailable", action: nil, keyEquivalent: "")
                }
            }
        }
        menu.addItem(.separator())
        menu.addItem(withTitle: "Enable Finder Menus…", action: #selector(enableFinder), keyEquivalent: "").target = self
        menu.addItem(withTitle: "Open Cloud Storage", action: #selector(openStorage), keyEquivalent: "").target = self
        menu.addItem(withTitle: "Reload Connection Menu", action: #selector(refreshConnections), keyEquivalent: "").target = self
        menu.addItem(.separator())
        menu.addItem(withTitle: "Quit RDrive", action: #selector(quit), keyEquivalent: "q").target = self
        updateRoots()
    }

    @objc func addDomain(_ sender: NSMenuItem) {
        guard let value = sender.representedObject as? [String: Any], let id = value["id"] as? String else { return }
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(id), displayName: value["display_name"] as? String ?? id)
        integration.addDomain(domain) { error in
            DispatchQueue.main.async {
                if let error = error { self.showMessage(error.localizedDescription) }
                else { self.updateRoots() }
            }
        }
    }
    @objc func refreshConnection(_ sender: NSMenuItem) {
        guard let id = sender.representedObject as? String, let bridge = bridge else { return }
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let reply = try self.integration.request(bridge, ["op": "refresh_index", "domain": id, "id": "root", "recursive": true])
                guard reply["queued"] as? Bool == true else {
                    throw NativeFailure.bridge("RDrive did not acknowledge the refresh request.")
                }
                DispatchQueue.main.async { self.showMessage("Connection index refresh queued.") }
            } catch {
                DispatchQueue.main.async { self.showMessage(providerError(error).localizedDescription) }
            }
        }
    }
    func updateRoots() {
        guard let bridge = bridge else { return }
        DispatchQueue.global(qos: .utility).async {
            guard let reply = try? self.integration.request(bridge, ["op": "domains"]), let domains = reply["domains"] as? [[String: Any]] else { return }
            // File Provider owns its native menus and decorations. Finder Sync
            // observes only explicitly mapped ordinary folders, avoiding two
            // extensions competing for the same CloudStorage namespace.
            let roots = domains.compactMap { domain -> WatchedRoot? in
                guard let id = domain["id"] as? String, let path = domain["local_root"] as? String else { return nil }
                return WatchedRoot(path: path, domain: id)
            }
            if let data = try? JSONEncoder().encode(roots) { try? data.write(to: bridge.paths.roots, options: .atomic) }
        }
    }
    func showStatus(domain: String, id: String) {
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                guard let bridge = self.bridge else { throw NativeFailure.bridge("RDrive is unavailable.") }
                let reply = try self.integration.request(bridge, ["op": "status", "domain": domain, "id": id])
                let state = reply["state"] as? String ?? "unknown"
                let detail = reply["message"] as? String ?? ""
                DispatchQueue.main.async { self.showMessage(state.capitalized + (detail.isEmpty ? "" : "\n\n" + detail)) }
            } catch { DispatchQueue.main.async { self.showMessage((error as NSError).localizedDescription) } }
        }
    }
    func showMessage(_ message: String) {
        integration.message(message)
    }
    @objc func enableFinder() { integration.enableFinder() }
    @objc func openStorage() { integration.openStorage() }
    @objc func refreshConnections() { rebuildMenu() }
    @objc func quit() { integration.quit() }
}

if let status = controlCommand() { exit(status) }
let app = NSApplication.shared
let delegate = CompanionDelegate()
app.delegate = delegate
app.run()
