import Foundation
import AppKit
import FinderSync
import FileProvider
import Darwin

func printJSON(_ value: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), let string = String(data: data, encoding: .utf8) { print(string) }
}

func option(_ name: String) -> String? {
    guard let index = CommandLine.arguments.firstIndex(of: name), index + 1 < CommandLine.arguments.count else { return nil }
    return CommandLine.arguments[index + 1]
}

func configuredPaths() throws -> NativePaths {
    if let path = option("--group-directory") {
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
    if let development = Bundle.main.object(forInfoDictionaryKey: "RDriveDevelopmentIPCRoot") as? String,
       !FileManager.default.fileExists(atPath: development) {
        try FileManager.default.createDirectory(atPath: development, withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700])
    }
    return try NativePaths.configured()
}

func runUntil(_ invoke: (@escaping (Error?) -> Void) -> Void) -> Int32 {
    let lock = NSLock()
    var finished = false
    var failure: Error?
    invoke { error in lock.lock(); failure = error; finished = true; lock.unlock() }
    let deadline = Date(timeIntervalSinceNow: 30)
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

func controlCommand() -> Int32? {
    if CommandLine.arguments.contains("--watch-domains") {
        do {
            let bridge = BackendBridge(paths: try configuredPaths())
            _ = try bridge.request(["op": "domains"])
            let watcher = NativeDomainWatcher(bridge: bridge)
            var running = true
            var sources: [DispatchSourceSignal] = []
            for number in [SIGINT, SIGTERM] {
                Darwin.signal(number, SIG_IGN)
                let source = DispatchSource.makeSignalSource(signal: number, queue: .main)
                source.setEventHandler { running = false }
                source.resume(); sources.append(source)
            }
            let timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in watcher.poll() }
            watcher.poll()
            printJSON(["ok": true, "event": "native_watcher_started"])
            while running { RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.1)) }
            timer.invalidate(); sources.forEach { $0.cancel() }; watcher.stop()
            printJSON(["ok": true, "event": "native_watcher_stopped"])
            return 0
        } catch { printJSON(["ok": false, "error": String(describing: error)]); return 1 }
    }
    if CommandLine.arguments.contains("--diagnose") {
        var report: [String: Any] = ["ok": true, "bundle_id": Bundle.main.bundleIdentifier ?? "", "finder_enabled": FIFinderSyncController.isExtensionEnabled]
        do { report["group_directory"] = try configuredPaths().group.path }
        catch { report["group_error"] = String(describing: error) }
        printJSON(report)
        return 0
    }
    if CommandLine.arguments.contains("--list-domains") {
        return runUntil { done in
            NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in
                if error == nil { printJSON(["ok": true, "domains": domains.map { ["id": $0.identifier.rawValue, "display_name": $0.displayName] }]) }
                done(error)
            }
        }
    }
    if let limitString = option("--trim-cache") {
        guard let limit = UInt64(limitString) else { printJSON(["ok": false, "error": "Pass a nonnegative cache byte limit."]); return 2 }
        return runUntil { done in
            DispatchQueue.global(qos: .utility).async {
                do {
                    let bridge = try BackendBridge(paths: configuredPaths())
                    try NativeMaterializationBudget(bridge: bridge, domain: "").trim(to: limit)
                    printJSON(["ok": true, "trimmed_to_max_total_bytes": limit]); done(nil)
                } catch { done(error) }
            }
        }
    }
    for action in ["--pin", "--unpin", "--check-cache"] {
        if let identifier = option(action) {
            guard let index = CommandLine.arguments.firstIndex(of: action), index + 2 < CommandLine.arguments.count else {
                printJSON(["ok": false, "error": "An item identifier is required."]); return 2
            }
            let id = CommandLine.arguments[index + 2]
            return runUntil { done in
                DispatchQueue.global(qos: .utility).async {
                    var reservation: [String]?
                    var budget: NativeMaterializationBudget?
                    do {
                        let bridge = BackendBridge(paths: try configuredPaths())
                        let activeBudget = NativeMaterializationBudget(bridge: bridge, domain: identifier); budget = activeBudget
                        if action != "--unpin" {
                            let item = try bridge.item(["op": "item", "domain": identifier, "id": id])
                            reservation = try activeBudget.reserve(item, pin: action == "--pin", operation: nil)
                        }
                        if action == "--check-cache" { activeBudget.release(reservation); printJSON(["ok": true, "cache_preflight": true]); done(nil); return }
                        _ = try bridge.request(["op": "pin", "domain": identifier, "id": id, "pinned": action == "--pin"])
                        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
                        NSFileProviderManager(for: domain)?.signalEnumerator(for: .workingSet) { _ in }
                        printJSON(["ok": true, "action": action, "domain": identifier, "id": id]); done(nil)
                    } catch { budget?.release(reservation); done(error) }
                }
            }
        }
    }
    if let identifier = option("--register-domain") {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: option("--display-name") ?? identifier)
        return runUntil { done in
            NSFileProviderManager.add(domain) { error in
                if error == nil { printJSON(["ok": true, "registered_domain": identifier]) }
                done(error)
            }
        }
    }
    if let identifier = option("--remove-domain") {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
        return runUntil { done in
            NSFileProviderManager.remove(domain, mode: .preserveDownloadedUserData) { preserved, error in
                if error == nil { printJSON(["ok": true, "removed_domain": identifier, "preserved_location": preserved?.path ?? ""]) }
                done(error)
            }
        }
    }
    if let identifier = option("--domain-root") {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
        return runUntil { done in
            guard let manager = NSFileProviderManager(for: domain) else { done(NativeFailure.invalid("The domain manager is unavailable.")); return }
            manager.getUserVisibleURL(for: .rootContainer) { url, error in
                if error == nil { printJSON(["ok": true, "path": url?.path ?? ""]) }
                done(error)
            }
        }
    }
    if let path = option("--resolve-file") {
        return runUntil { done in
            NSFileProviderManager.getIdentifierForUserVisibleFile(at: URL(fileURLWithPath: path)) { item, domain, error in
                if error == nil { printJSON(["ok": true, "id": item.map(wireID) ?? "", "domain": domain?.rawValue ?? ""]) }
                done(error)
            }
        }
    }
    if let path = option("--set-tags"), let index = CommandLine.arguments.firstIndex(of: "--set-tags") {
        let names = Array(CommandLine.arguments.dropFirst(index + 2))
        var coordinationError: NSError?
        var failure: Error?
        NSFileCoordinator().coordinate(writingItemAt: URL(fileURLWithPath: path), options: .contentIndependentMetadataOnly, error: &coordinationError) { coordinated in
            do {
                try (coordinated as NSURL).setResourceValue(names, forKey: .tagNamesKey)
            } catch { failure = error }
        }
        if let error = failure ?? coordinationError { printJSON(["ok": false, "error": (error as NSError).localizedDescription]); return 1 }
        printJSON(["ok": true, "tags": names]); return 0
    }
    for action in ["--signal", "--download", "--evict"] {
        if let identifier = option(action) {
            let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identifier), displayName: identifier)
            return runUntil { done in
                guard let manager = NSFileProviderManager(for: domain) else { done(NativeFailure.invalid("The domain manager is unavailable.")); return }
                let complete: (Error?) -> Void = { error in
                    if error == nil { printJSON(["ok": true, "action": action, "domain": identifier]) }
                    done(error)
                }
                if action == "--signal" { manager.signalEnumerator(for: .workingSet, completionHandler: complete); return }
                guard let index = CommandLine.arguments.firstIndex(of: action), index + 2 < CommandLine.arguments.count else {
                    done(NativeFailure.invalid("An item identifier is required.")); return
                }
                let item = providerID(CommandLine.arguments[index + 2])
                if action == "--download" { manager.requestDownloadForItem(withIdentifier: item, completionHandler: complete) }
                else { manager.evictItem(identifier: item, completionHandler: complete) }
            }
        }
    }
    if CommandLine.arguments.contains("--enable-finder") {
        FIFinderSyncController.showExtensionManagementInterface()
        printJSON(["ok": true, "opened_extension_settings": true])
        return 0
    }
    let allowedGUI = Set(["--group-directory", "--show-status", "--show-message"])
    if let unknown = CommandLine.arguments.dropFirst().first(where: { $0.hasPrefix("--") && !allowedGUI.contains($0) }) {
        printJSON(["ok": false, "error": "Unknown native control option: " + unknown]); return 2
    }
    return nil
}

final class CompanionDelegate: NSObject, NSApplicationDelegate {
    private var status: NSStatusItem?
    private var service: Process?
    private let serviceLock = NSLock()
    private var terminating = false
    private var bridge: BackendBridge?
    private var refresh: Timer?
    private var catalogRefresh: Timer?
    private var watcher: NativeDomainWatcher?
    private let menu = NSMenu()

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        status = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        status?.button?.image = NSImage(systemSymbolName: "externaldrive.connected.to.line.below", accessibilityDescription: "RDrive")
        status?.menu = menu
        do {
            let paths = try configuredPaths()
            try paths.prepare()
            bridge = BackendBridge(paths: paths)
            watcher = NativeDomainWatcher(bridge: bridge!)
            startService(paths)
            rebuildMenu()
            refresh = Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { [weak self] _ in self?.rebuildMenu() }
            catalogRefresh = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in self?.watcher?.poll() }
        } catch {
            menu.addItem(withTitle: "Connection unavailable", action: nil, keyEquivalent: "")
            menu.addItem(withTitle: "Quit RDrive", action: #selector(quit), keyEquivalent: "q").target = self
            showMessage((error as NSError).localizedDescription)
        }
        if let domain = option("--show-status"),
           let index = CommandLine.arguments.firstIndex(of: "--show-status"), index + 2 < CommandLine.arguments.count {
            showStatus(domain: domain, id: CommandLine.arguments[index + 2])
        }
        if let message = option("--show-message") { showMessage(message) }
    }

    func applicationWillTerminate(_ notification: Notification) {
        refresh?.invalidate()
        catalogRefresh?.invalidate()
        watcher?.stop()
        serviceLock.lock(); terminating = true; let owned = service; serviceLock.unlock()
        if let owned = owned, owned.isRunning { _ = Darwin.kill(owned.processIdentifier, SIGINT) }
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            guard url.scheme == "rdrive-native", let components = URLComponents(url: url, resolvingAgainstBaseURL: false) else { continue }
            let values = Dictionary((components.queryItems ?? []).map { ($0.name, $0.value ?? "") }, uniquingKeysWith: { _, last in last })
            if url.host == "status", let domain = values["domain"], let id = values["id"] { showStatus(domain: domain, id: id) }
            else if url.host == "message", let message = values["text"] { showMessage(message) }
        }
    }

    private func startService(_ paths: NativePaths) {
        DispatchQueue.global(qos: .utility).async {
            if (try? self.bridge?.request(["op": "domains"])) != nil { return }
            let process = Process()
            process.executableURL = Bundle.main.resourceURL?.appendingPathComponent("rdrive")
            var arguments = ["provider", "serve", "--socket", paths.socket.path, "--staging", paths.transfers.path]
            if paths.development { arguments += ["--development-tcp"] }
            if let home = ProcessInfo.processInfo.environment["RDRIVE_HOME"] { arguments += ["--home", home] }
            process.arguments = arguments
            process.standardOutput = FileHandle.nullDevice
            process.standardError = FileHandle.nullDevice
            do {
                self.serviceLock.lock(); defer { self.serviceLock.unlock() }
                if self.terminating { return }
                try process.run(); self.service = process
            }
            catch { DispatchQueue.main.async { self.showMessage("RDrive could not start its connection service.") } }
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self.rebuildMenu() }
        }
    }

    private func rebuildMenu() {
        menu.removeAllItems()
        menu.addItem(withTitle: "RDrive Connections", action: nil, keyEquivalent: "")
        let connections = NSMenu(title: "Add Connection to Finder")
        connections.addItem(withTitle: "Loading connections…", action: nil, keyEquivalent: "")
        let parent = NSMenuItem(title: "Add Connection to Finder", action: nil, keyEquivalent: "")
        parent.submenu = connections; menu.addItem(parent)
        DispatchQueue.global(qos: .utility).async {
            let reply = try? self.bridge?.request(["op": "domains"])
            DispatchQueue.main.async {
                connections.removeAllItems()
                if let domains = reply?["domains"] as? [[String: Any]] {
                    for domain in domains {
                        guard let id = domain["id"] as? String else { continue }
                        let item = NSMenuItem(title: domain["display_name"] as? String ?? id, action: #selector(self.addDomain(_:)), keyEquivalent: "")
                        item.target = self; item.representedObject = domain; connections.addItem(item)
                    }
                    if domains.isEmpty { connections.addItem(withTitle: "No connections configured", action: nil, keyEquivalent: "") }
                } else { connections.addItem(withTitle: "Connection service unavailable", action: nil, keyEquivalent: "") }
            }
        }
        menu.addItem(.separator())
        menu.addItem(withTitle: "Enable Finder Menus…", action: #selector(enableFinder), keyEquivalent: "").target = self
        menu.addItem(withTitle: "Open Cloud Storage", action: #selector(openStorage), keyEquivalent: "").target = self
        menu.addItem(withTitle: "Refresh Connections", action: #selector(refreshConnections), keyEquivalent: "").target = self
        menu.addItem(.separator())
        menu.addItem(withTitle: "Quit RDrive", action: #selector(quit), keyEquivalent: "q").target = self
        updateRoots()
    }

    @objc private func addDomain(_ sender: NSMenuItem) {
        guard let value = sender.representedObject as? [String: Any], let id = value["id"] as? String else { return }
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(id), displayName: value["display_name"] as? String ?? id)
        NSFileProviderManager.add(domain) { error in
            DispatchQueue.main.async {
                if let error = error { self.showMessage(error.localizedDescription) }
                else { self.updateRoots() }
            }
        }
    }
    private func updateRoots() {
        guard let bridge = bridge else { return }
        DispatchQueue.global(qos: .utility).async {
            guard let reply = try? bridge.request(["op": "domains"]), let domains = reply["domains"] as? [[String: Any]] else { return }
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
    private func showStatus(domain: String, id: String) {
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                guard let reply = try self.bridge?.request(["op": "status", "domain": domain, "id": id]) else { throw NativeFailure.bridge("RDrive is unavailable.") }
                let state = reply["state"] as? String ?? "unknown"
                let detail = reply["message"] as? String ?? ""
                DispatchQueue.main.async { self.showMessage(state.capitalized + (detail.isEmpty ? "" : "\n\n" + detail)) }
            } catch { DispatchQueue.main.async { self.showMessage((error as NSError).localizedDescription) } }
        }
    }
    private func showMessage(_ message: String) {
        let alert = NSAlert(); alert.messageText = "RDrive"; alert.informativeText = message
        alert.addButton(withTitle: "OK"); NSApp.activate(ignoringOtherApps: true); alert.runModal()
    }
    @objc private func enableFinder() { FIFinderSyncController.showExtensionManagementInterface() }
    @objc private func openStorage() { NSWorkspace.shared.open(FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/CloudStorage")) }
    @objc private func refreshConnections() { rebuildMenu() }
    @objc private func quit() { NSApp.terminate(nil) }
}

if let status = controlCommand() { exit(status) }
let app = NSApplication.shared
let delegate = CompanionDelegate()
app.delegate = delegate
app.run()
