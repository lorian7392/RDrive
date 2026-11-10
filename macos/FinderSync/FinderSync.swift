import Foundation
import AppKit
import FinderSync
import FileProvider
import Darwin

@objc(RDriveFinderSync)
final class RDriveFinderSync: FIFinderSync {
    private var bridge: BackendBridge?
    private var roots: [WatchedRoot] = []
    private var visible: Set<URL> = []
    private var pending: Set<URL> = []
    private var timer: DispatchSourceTimer?
    private var monitored: Set<URL> = []
    private let queue = DispatchQueue(label: "io.rdrive.finder.badges", qos: .utility)

    override init() {
        super.init()
        do { bridge = try BackendBridge(bundle: Bundle(for: RDriveFinderSync.self)) }
        catch { nativeDevelopmentLog("Finder Sync bridge initialization: \(error)") }
        nativeDevelopmentLog("Finder Sync initialized")
        let controller = FIFinderSyncController.default()
        for (id, symbol, label) in [
            ("online", "icloud", "Online only"), ("cached", "checkmark.circle", "Downloaded"),
            ("pinned", "pin.circle.fill", "Kept offline"), ("syncing", "arrow.triangle.2.circlepath", "Syncing"),
            ("error", "exclamationmark.circle", "Needs attention")
        ] {
            if let image = NSImage(systemSymbolName: symbol, accessibilityDescription: label) {
                controller.setBadgeImage(image, label: label, forBadgeIdentifier: id)
            }
        }
        reloadRoots()
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 1, repeating: 5)
        timer.setEventHandler { [weak self] in
            guard let self = self else { return }
            self.reloadRoots()
            for url in self.visible { self.updateBadge(url) }
        }
        timer.resume(); self.timer = timer
    }
    deinit { timer?.cancel() }

    private func reloadRoots() {
        guard let path = bridge?.paths.roots,
              let data = try? Data(contentsOf: path), let roots = try? JSONDecoder().decode([WatchedRoot].self, from: data) else {
            self.roots = []
            if !monitored.isEmpty { monitored = []; FIFinderSyncController.default().directoryURLs = [] }
            return
        }
        self.roots = roots
        let next = Set(roots.map { URL(fileURLWithPath: $0.path, isDirectory: true).standardizedFileURL })
        if next != monitored {
            monitored = next
            FIFinderSyncController.default().directoryURLs = next
            nativeDevelopmentLog("Finder Sync monitored root count=\(next.count)")
        }
    }
    override func beginObservingDirectory(at url: URL) { visible.insert(url); updateBadge(url) }
    override func endObservingDirectory(at url: URL) {
        visible = visible.filter { $0 != url && !$0.path.hasPrefix(url.path + "/") }
    }
    override func requestBadgeIdentifier(for url: URL) {
        if visible.count < 10000 { visible.insert(url) }
        updateBadge(url)
    }
    private func resolve(_ url: URL, completion: @escaping (Result<(String, String, Bool), Error>) -> Void) {
        let configuredDomains = Set(roots.map { $0.domain })
        NSFileProviderManager.getIdentifierForUserVisibleFile(at: url) { item, domain, _ in
            if let item = item, let domain = domain, configuredDomains.contains(domain.rawValue) {
                completion(.success((domain.rawValue, wireID(item), true)))
                return
            }
            self.queue.async {
                do {
                    guard let bridge = self.bridge else { throw NativeFailure.bridge("The companion is unavailable.") }
                    let reply = try bridge.request(["op": "resolve", "file_url": url.absoluteString])
                    guard let domain = reply["domain"] as? String, let id = reply["id"] as? String else {
                        throw NativeFailure.invalid("This path is not mapped to an RDrive connection.")
                    }
                    completion(.success((domain, id, false)))
                } catch { completion(.failure(error)) }
            }
        }
    }
    private func updateBadge(_ url: URL) {
        DispatchQueue.main.async { self.updateVisibleBadge(url) }
    }
    private func updateVisibleBadge(_ url: URL) {
        guard visible.contains(url), !pending.contains(url) else { return }
        pending.insert(url)
        resolve(url) { result in
            self.queue.async {
                var badge = "error"
                var nativeDomain = false
                if case .success(let (domain, id, isNative)) = result,
                   let reply = try? self.bridge?.request(["op": "status", "domain": domain, "id": id]),
                   let state = reply["state"] as? String,
                   let value = NativeSyncState(rawValue: state) {
                    badge = value == .queued ? "syncing" : (value == .conflict ? "error" : value.rawValue)
                    nativeDomain = isNative
                }
                if nativeDomain, let dataless = self.isDatalessRegularFile(url) {
                    if badge == "pinned" && dataless { badge = "syncing" }
                    else if badge == "cached" && dataless { badge = "online" }
                    else if badge == "online" && !dataless { badge = "cached" }
                }
                DispatchQueue.main.async {
                    self.pending.remove(url)
                    if self.visible.contains(url) { FIFinderSyncController.default().setBadgeIdentifier(badge, for: url) }
                }
            }
        }
    }
    private func isDatalessRegularFile(_ url: URL) -> Bool? {
        url.withUnsafeFileSystemRepresentation { path -> Bool? in
            guard let path = path else { return nil }
            var metadata = stat()
            guard lstat(path, &metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { return nil }
            return metadata.st_flags & UInt32(SF_DATALESS) != 0
        }
    }
    override func menu(for menuKind: FIMenuKind) -> NSMenu? {
        let controller = FIFinderSyncController.default()
        let selected = controller.selectedItemURLs() ?? controller.targetedURL().map { [$0] } ?? []
        guard !selected.isEmpty else { return nil }
        let menu = NSMenu(title: "RDrive")
        for (title, action) in [
            ("Keep Offline", #selector(pin(_:))), ("Allow Automatic Removal", #selector(unpin(_:))),
            ("Download Now", #selector(download(_:))), ("Remove Download", #selector(evict(_:))),
            ("Show Sync Status", #selector(status(_:)))
        ] {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
            item.target = self; item.representedObject = selected; menu.addItem(item)
        }
        return menu
    }
    override var toolbarItemName: String { "RDrive" }
    override var toolbarItemToolTip: String { "RDrive file availability" }
    override var toolbarItemImage: NSImage { NSImage(systemSymbolName: "externaldrive.connected.to.line.below", accessibilityDescription: "RDrive") ?? NSImage(size: NSSize(width: 18, height: 18)) }

    @objc private func pin(_ sender: NSMenuItem) { act(sender, action: "pin") }
    @objc private func unpin(_ sender: NSMenuItem) { act(sender, action: "unpin") }
    @objc private func download(_ sender: NSMenuItem) { act(sender, action: "download") }
    @objc private func evict(_ sender: NSMenuItem) { act(sender, action: "evict") }
    @objc private func status(_ sender: NSMenuItem) { act(sender, action: "status") }

    private func act(_ sender: NSMenuItem, action: String) {
        guard let urls = sender.representedObject as? [URL] else { return }
        for url in urls {
            resolve(url) { result in
                self.queue.async {
                    do {
                        let (domain, id, isNative) = try result.get()
                        let providerDomain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(domain), displayName: domain)
                        let manager = NSFileProviderManager(for: providerDomain)
                        if action == "status" { self.showCompanion(arguments: ["--show-status", domain, id]); return }
                        guard isNative else {
                            throw NativeFailure.invalid("Use the Smart Finder view for offline storage. A network-mounted file cannot be kept offline by changing its RDrive policy.")
                        }
                        if action == "pin" || action == "unpin" {
                            guard let bridge = self.bridge else { throw NativeFailure.bridge("The companion is unavailable.") }
                            _ = try bridge.request(["op": "pin", "domain": domain, "id": id, "pinned": action == "pin"])
                            manager?.signalEnumerator(for: .workingSet) { _ in }
                            self.updateBadge(url)
                        } else if action == "download" {
                            guard let manager = manager else { throw NativeFailure.invalid("The File Provider domain is not registered.") }
                            manager.requestDownloadForItem(withIdentifier: providerID(id)) { error in
                                if let error = error { self.showError(error) }
                            }
                        } else if action == "evict" {
                            guard let manager = manager else { throw NativeFailure.invalid("The File Provider domain is not registered.") }
                            manager.evictItem(identifier: providerID(id)) { error in
                                if let error = error { self.showError(error) }
                            }
                        }
                    } catch { self.showError(error) }
                }
            }
        }
    }
    private func showError(_ error: Error) { showCompanion(arguments: ["--show-message", (error as NSError).localizedDescription]) }
    private func showCompanion(arguments: [String]) {
        var components = URLComponents()
        components.scheme = "rdrive-native"
        if arguments.first == "--show-status", arguments.count == 3 {
            components.host = "status"; components.queryItems = [URLQueryItem(name: "domain", value: arguments[1]), URLQueryItem(name: "id", value: arguments[2])]
        } else if arguments.first == "--show-message", arguments.count == 2 {
            components.host = "message"; components.queryItems = [URLQueryItem(name: "text", value: arguments[1])]
        }
        guard let url = components.url else { return }
        DispatchQueue.main.async {
            NSWorkspace.shared.open(url)
        }
    }
}
