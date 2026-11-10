import Foundation
import AppKit
import FileProvider
import FileProviderUI

@objc(RDriveStatusViewController)
final class RDriveStatusViewController: FPUIActionExtensionViewController {
    private let heading = NSTextField(labelWithString: "Sync Status")
    private let details = NSTextField(wrappingLabelWithString: "Checking file availability…")
    private var operation: SocketOperation?

    override func loadView() {
        view = NSView(frame: NSRect(x: 0, y: 0, width: 480, height: 260))
        heading.font = .boldSystemFont(ofSize: 18)
        details.maximumNumberOfLines = 12
        details.isSelectable = true
        let done = NSButton(title: "Done", target: self, action: #selector(finish))
        done.keyEquivalent = "\r"
        let stack = NSStackView(views: [heading, details, done])
        stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 16
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 24),
            stack.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -24),
            stack.topAnchor.constraint(equalTo: view.topAnchor, constant: 24),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: view.bottomAnchor, constant: -24)
        ])
        preferredContentSize = view.frame.size
    }

    override func prepare(forAction actionIdentifier: String, itemIdentifiers: [NSFileProviderItemIdentifier]) {
        _ = view
        guard actionIdentifier == "rdrive.status", let domain = extensionContext.domainIdentifier,
              !itemIdentifiers.isEmpty, itemIdentifiers.count <= 128 else {
            details.stringValue = "Select RDrive files to view their sync status."
            return
        }
        nativeDevelopmentLog("Status context domain=\(domain.rawValue) itemIDs=\(itemIdentifiers.map { $0.rawValue }.joined(separator: ","))")
        let operation = SocketOperation(); self.operation = operation
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let bridge = try BackendBridge(bundle: Bundle(for: RDriveStatusViewController.self))
                var lines: [String] = []
                for identifier in itemIdentifiers.prefix(10) {
                    try operation.check()
                    let canonical = try self.canonicalIdentifier(identifier, domain: domain)
                    let item = try bridge.item(["op": "item", "domain": domain.rawValue, "id": wireID(canonical)], operation: operation)
                    let state = NativeSyncState.item(item)
                    var line = item.name + ": " + state.title
                    if item.smart == true { line += " · Smart Synchronization" }
                    if let message = item.sync_message, !message.isEmpty { line += "\n" + String(message.prefix(500)) }
                    lines.append(line)
                }
                if itemIdentifiers.count > 10 { lines.append("And \(itemIdentifiers.count - 10) more selected files.") }
                DispatchQueue.main.async { self.details.stringValue = lines.joined(separator: "\n\n") }
            } catch {
                DispatchQueue.main.async { self.details.stringValue = "Status unavailable: " + (error as NSError).localizedDescription }
            }
        }
    }

    private func canonicalIdentifier(_ identifier: NSFileProviderItemIdentifier, domain: NSFileProviderDomainIdentifier) throws -> NSFileProviderItemIdentifier {
        guard identifier.rawValue.hasPrefix("__fp/fs/") else { return identifier }
        let nativeDomain = NSFileProviderDomain(identifier: domain, displayName: domain.rawValue)
        guard let manager = NSFileProviderManager(for: nativeDomain) else { throw NativeFailure.invalid("The selected native domain is unavailable.") }
        let located = DispatchSemaphore(value: 0)
        var url: URL?; var failure: Error?
        manager.getUserVisibleURL(for: identifier) { url = $0; failure = $1; located.signal() }
        guard located.wait(timeout: .now() + 20) == .success else { throw NativeFailure.bridge("The selected file could not be located in time.") }
        if let failure = failure { throw failure }
        guard let url = url else { throw NativeFailure.invalid("The selected file is unavailable.") }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let identified = DispatchSemaphore(value: 0)
        var item: NSFileProviderItemIdentifier?; var resolvedDomain: NSFileProviderDomainIdentifier?
        failure = nil
        NSFileProviderManager.getIdentifierForUserVisibleFile(at: url) {
            item = $0; resolvedDomain = $1; failure = $2; identified.signal()
        }
        guard identified.wait(timeout: .now() + 20) == .success else { throw NativeFailure.bridge("The selected file's identity request timed out.") }
        if let failure = failure { throw failure }
        guard resolvedDomain == domain, let item = item, !item.rawValue.hasPrefix("__fp/fs/") else {
            throw NativeFailure.invalid("The selected file could not be matched to this storage connection.")
        }
        return item
    }

    override func prepare(forError error: Error) {
        _ = view
        details.stringValue = (error as NSError).localizedDescription
    }

    @objc private func finish() {
        operation?.cancel(); operation = nil
        extensionContext.completeRequest()
    }
    deinit { operation?.cancel() }
}
