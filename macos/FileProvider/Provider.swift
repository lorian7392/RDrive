import Foundation
import FileProvider
import UniformTypeIdentifiers

private final class CompletionOnce<T> {
    private let lock = NSLock()
    private var callback: ((Result<T, Error>) -> Void)?
    init(_ callback: @escaping (Result<T, Error>) -> Void) { self.callback = callback }
    @discardableResult func finish(_ value: Result<T, Error>) -> Bool {
        lock.lock(); let callback = self.callback; self.callback = nil; lock.unlock()
        callback?(value)
        return callback != nil
    }
}

@objc(RDriveProvider)
final class RDriveProvider: NSObject, NSFileProviderReplicatedExtension, NSFileProviderCustomAction {
    let domain: NSFileProviderDomain
    private let bridge: BackendBridge?
    private let manager: NSFileProviderManager?
    private let lock = NSLock()
    private var active: [UUID: SocketOperation] = [:]
    private var invalidated = false
    private var timer: DispatchSourceTimer?
    private var budgetSweepInFlight = false

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        do { bridge = try BackendBridge(bundle: Bundle(for: RDriveProvider.self)) }
        catch { bridge = nil; nativeDevelopmentLog("Provider initialization: \(error)") }
        manager = NSFileProviderManager(for: domain)
        super.init()
        let timer = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
        timer.schedule(deadline: .now() + 15, repeating: 15)
        timer.setEventHandler { [weak self] in self?.signal(); self?.sweepBudget() }
        timer.resume()
        self.timer = timer
        if let directory = try? manager?.temporaryDirectoryURL() {
            DispatchQueue.global(qos: .utility).async {
                let urls = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
                for url in urls where url.lastPathComponent.hasPrefix("rdrive-fetch-") {
                    if let modified = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
                       modified < Date(timeIntervalSinceNow: -86400) { try? FileManager.default.removeItem(at: url) }
                }
            }
        }
    }

    func invalidate() {
        timer?.cancel(); timer = nil
        lock.lock(); invalidated = true; let pending = Array(active.values); active.removeAll(); lock.unlock()
        pending.forEach { $0.cancel() }
    }

    func materializedItemsDidChange(completionHandler: @escaping () -> Void) {
        sweepBudget()
        completionHandler()
    }

    private func signal(_ parent: NSFileProviderItemIdentifier? = nil) {
        manager?.signalEnumerator(for: .workingSet) { _ in }
        if let parent = parent, parent != .workingSet { manager?.signalEnumerator(for: parent) { _ in } }
    }

    private func sweepBudget() {
        lock.lock()
        guard !invalidated, !budgetSweepInFlight, let bridge = bridge else { lock.unlock(); return }
        budgetSweepInFlight = true; lock.unlock()
        DispatchQueue.global(qos: .utility).async {
            defer { self.lock.lock(); self.budgetSweepInFlight = false; self.lock.unlock() }
            do { try NativeMaterializationBudget(bridge: bridge, domain: self.domain.identifier.rawValue).sweep() }
            catch { nativeDevelopmentLog("Native cache sweep: \(error)") }
        }
    }

    private func work<T>(_ body: @escaping (BackendBridge, SocketOperation) throws -> T,
                         discard: ((T) -> Void)? = nil,
                         completion: @escaping (Result<T, Error>) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        let operation = SocketOperation(progress: progress)
        let once = CompletionOnce(completion)
        let token = UUID()
        lock.lock(); let closed = invalidated; active[token] = operation; lock.unlock()
        progress.cancellationHandler = {
            operation.cancel()
            once.finish(.failure(NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError)))
        }
        DispatchQueue.global(qos: .userInitiated).async {
            defer { self.lock.lock(); self.active.removeValue(forKey: token); self.lock.unlock() }
            do {
                if closed { throw NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError) }
                guard let bridge = self.bridge else { throw NativeFailure.invalid("The provider's App Group is unavailable; check signing and entitlements.") }
                try operation.check()
                let result = try body(bridge, operation)
                do { try operation.check() } catch { discard?(result); throw error }
                progress.completedUnitCount = progress.totalUnitCount
                if !once.finish(.success(result)) { discard?(result) }
            } catch {
                nativeDevelopmentLog("Provider callback: \(error)")
                once.finish(.failure(providerError(error)))
            }
        }
        return progress
    }

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        if containerItemIdentifier == .trashContainer { throw remoteError(["code": "unsupported", "message": "This provider does not implement a remote trash."]) }
        guard let bridge = bridge else { throw providerError(NativeFailure.invalid("The provider's App Group is unavailable.")) }
        return ProviderEnumerator(domain: domain.identifier.rawValue, container: containerItemIdentifier, bridge: bridge)
    }

    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        work({ bridge, operation in
            ProviderItem(try bridge.item(["op": "item", "domain": self.domain.identifier.rawValue, "id": wireID(identifier)], operation: operation))
        }) { result in
            switch result { case .success(let item): completionHandler(item, nil); case .failure(let error): completionHandler(nil, error) }
        }
    }

    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest, completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        work({ bridge, operation -> (URL, ProviderItem, () -> Void) in
            let expected = try bridge.item(["op": "item", "domain": self.domain.identifier.rawValue, "id": wireID(itemIdentifier)], operation: operation)
            let budget = NativeMaterializationBudget(bridge: bridge, domain: self.domain.identifier.rawValue)
            let reservation: [String]?
            do { reservation = try budget.reserve(expected, operation: operation) }
            catch {
                let failure = error as NSError
                if failure.domain == NSFileProviderErrorDomain && failure.code == NSFileProviderError.insufficientQuota.rawValue {
                    _ = try? bridge.request(["op": "native_error", "domain": self.domain.identifier.rawValue, "id": wireID(itemIdentifier),
                        "base_content_version": expected.content_version, "native_message": failure.localizedDescription])
                    self.signal()
                }
                throw error
            }
            var delivered = false
            let heartbeat = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
            heartbeat.schedule(deadline: .now() + 30, repeating: 30)
            heartbeat.setEventHandler {
                do { try budget.renew(reservation) } catch { operation.cancel() }
            }
            heartbeat.resume()
            defer { heartbeat.cancel(); if !delivered { budget.release(reservation) } }
            var message: [String: Any] = ["op": "fetch", "domain": self.domain.identifier.rawValue, "id": wireID(itemIdentifier), "native_materialization": true]
            if let version = requestedVersion { message["base_content_version"] = try self.component(version.contentVersion) }
            let reply = try bridge.request(message, operation: operation)
            guard let path = reply["content_path"] as? String, let raw = reply["item"], let manager = self.manager else {
                throw NativeFailure.invalid("The download response or provider manager is missing.")
            }
            let item = ProviderItem(try WireItem.decode(raw))
            let source = URL(fileURLWithPath: path).standardizedFileURL
            let staging = bridge.paths.transfers.standardizedFileURL.path + "/"
            guard source.path.hasPrefix(staging) else { throw NativeFailure.invalid("Backend returned content outside its transfer staging directory.") }
            let values = try source.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
            guard values.isRegularFile == true, values.isSymbolicLink != true else { throw NativeFailure.invalid("Backend download is not a regular file.") }
            defer { try? FileManager.default.removeItem(at: source) }
            let destination = try manager.temporaryDirectoryURL().appendingPathComponent("rdrive-fetch-" + UUID().uuidString)
            do { try FileManager.default.copyItem(at: source, to: destination) }
            catch { try? FileManager.default.removeItem(at: destination); throw error }
            // The system clones/unlinks this real downloaded file. No placeholder
            // files are manufactured by the extension; macOS creates dataless items.
            try budget.markDelivered(reservation)
            _ = try? bridge.request(["op": "native_error", "domain": self.domain.identifier.rawValue, "id": wireID(itemIdentifier),
                "base_content_version": item.wire.content_version, "native_message": NSNull()])
            delivered = true
            return (destination, item, { budget.release(reservation) })
        }, discard: { pair in try? FileManager.default.removeItem(at: pair.0); pair.2() }) { result in
            switch result { case .success(let pair): completionHandler(pair.0, pair.1, nil); case .failure(let error): completionHandler(nil, nil, error) }
        }
    }

    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
                    options: NSFileProviderCreateItemOptions, request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        work({ bridge, operation in
            if itemTemplate.contentType == .symbolicLink { throw remoteError(["code": "unsupported", "message": "Remote symbolic links are not supported."]) }
            let kind = itemTemplate.contentType == .folder ? "directory" : "file"
            if kind == "file" && url == nil && options.contains(.mayAlreadyExist) {
                throw remoteError(["code": "unsupported", "message": "A dataless reimport requires content identity confirmation before creation."])
            }
            var message: [String: Any] = ["op": "create", "domain": self.domain.identifier.rawValue,
                "parent_id": wireID(itemTemplate.parentItemIdentifier), "name": itemTemplate.filename,
                "kind": kind, "client_item_id": itemTemplate.itemIdentifier.rawValue,
                "may_already_exist": options.contains(.mayAlreadyExist), "deletion_conflicted": options.contains(.deletionConflicted)]
            if fields.contains(.tagData) { message["tag_data_hex"] = try nativeFinderTagData(itemTemplate.tagData ?? nil)?.hexEncodedString() ?? "" }
            if fields.contains(.extendedAttributes) { message["extended_attributes"] = try nativeAttributePayload(itemTemplate.extendedAttributes ?? [:]) }
            var staged: URL?
            if let source = url { staged = try bridge.stagedCopy(source); message["content_path"] = staged!.path }
            defer { if let staged = staged { try? FileManager.default.removeItem(at: staged) } }
            let item = ProviderItem(try bridge.item(message, operation: operation))
            try? NativeMaterializationBudget(bridge: bridge, domain: self.domain.identifier.rawValue).acknowledgeLocalContent(item.wire)
            self.signal(item.parentItemIdentifier)
            return item
        }) { result in
            switch result { case .success(let item): completionHandler(item, [], false, nil); case .failure(let error): completionHandler(nil, fields, false, error) }
        }
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion, changedFields: NSFileProviderItemFields,
                    contents newContents: URL?, options: NSFileProviderModifyItemOptions, request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        work({ bridge, operation in
            let previousParent = (try? bridge.item(["op": "item", "domain": self.domain.identifier.rawValue,
                "id": wireID(item.itemIdentifier)], operation: operation)).map { ProviderItem($0).parentItemIdentifier }
            var message: [String: Any] = ["op": "modify", "domain": self.domain.identifier.rawValue,
                "id": wireID(item.itemIdentifier), "base_content_version": try self.component(version.contentVersion),
                "base_metadata_version": try self.component(version.metadataVersion)]
            if changedFields.contains(.filename) { message["name"] = item.filename }
            if changedFields.contains(.parentItemIdentifier) { message["parent_id"] = wireID(item.parentItemIdentifier) }
            if changedFields.contains(.tagData) {
                let tagData = item.tagData ?? nil
                nativeDevelopmentLog("Native tag callback: \(tagData?.count ?? 0) bytes; fields \(changedFields.rawValue)")
                message["tag_data_hex"] = try nativeFinderTagData(tagData)?.hexEncodedString() ?? ""
            }
            if changedFields.contains(.extendedAttributes) { message["extended_attributes"] = try nativeAttributePayload(item.extendedAttributes ?? [:]) }
            var staged: URL?
            if changedFields.contains(.contents) {
                guard let source = newContents else { throw NativeFailure.invalid("A content edit did not include its contents.") }
                staged = try bridge.stagedCopy(source); message["content_path"] = staged!.path
            }
            defer { if let staged = staged { try? FileManager.default.removeItem(at: staged) } }
            let updated = ProviderItem(try bridge.item(message, operation: operation))
            if newContents != nil { try? NativeMaterializationBudget(bridge: bridge, domain: self.domain.identifier.rawValue).acknowledgeLocalContent(updated.wire) }
            self.signal(updated.parentItemIdentifier)
            if let previousParent = previousParent, previousParent != updated.parentItemIdentifier { self.signal(previousParent) }
            return updated
        }) { result in
            switch result { case .success(let item): completionHandler(item, [], false, nil); case .failure(let error): completionHandler(nil, changedFields, false, error) }
        }
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions, request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        work({ bridge, operation in
            _ = try bridge.request(["op": "delete", "domain": self.domain.identifier.rawValue, "id": wireID(identifier),
                "base_content_version": try self.component(version.contentVersion), "base_metadata_version": try self.component(version.metadataVersion),
                "recursive": options.contains(.recursive)], operation: operation)
            self.signal()
        }) { result in
            switch result { case .success: completionHandler(nil); case .failure(let error): completionHandler(error) }
        }
    }

    func performAction(identifier actionIdentifier: NSFileProviderExtensionActionIdentifier,
                       onItemsWithIdentifiers itemIdentifiers: [NSFileProviderItemIdentifier],
                       completionHandler: @escaping (Error?) -> Void) -> Progress {
        work({ bridge, operation in
            guard ["rdrive.pin", "rdrive.unpin", "rdrive.download", "rdrive.evict"].contains(actionIdentifier.rawValue) else { throw remoteError(["code": "unsupported"]) }
            for identifier in itemIdentifiers {
                try operation.check()
                if actionIdentifier.rawValue == "rdrive.pin" || actionIdentifier.rawValue == "rdrive.unpin" {
                    let budget = NativeMaterializationBudget(bridge: bridge, domain: self.domain.identifier.rawValue)
                    var reservation: [String]?
                    if actionIdentifier.rawValue == "rdrive.pin" {
                        let item = try bridge.item(["op": "item", "domain": self.domain.identifier.rawValue, "id": wireID(identifier)], operation: operation)
                        reservation = try budget.reserve(item, pin: true, operation: operation)
                    }
                    do {
                        _ = try bridge.request(["op": "pin", "domain": self.domain.identifier.rawValue,
                            "id": wireID(identifier), "pinned": actionIdentifier.rawValue == "rdrive.pin"], operation: operation)
                    } catch { budget.release(reservation); throw error }
                } else {
                    guard let manager = self.manager else { throw NativeFailure.invalid("The File Provider domain is unavailable.") }
                    let done = DispatchSemaphore(value: 0)
                    var failure: Error?
                    let complete: (Error?) -> Void = { failure = $0; done.signal() }
                    if actionIdentifier.rawValue == "rdrive.download" {
                        manager.requestDownloadForItem(withIdentifier: identifier) { complete($0) }
                    } else { manager.evictItem(identifier: identifier) { complete($0) } }
                    guard done.wait(timeout: .now() + 30) == .success else { throw NativeFailure.bridge("The system download or eviction request timed out.") }
                    try operation.check()
                    if let failure = failure { throw failure }
                }
            }
            self.signal()
        }) { result in
            switch result { case .success: completionHandler(nil); case .failure(let error): completionHandler(error) }
        }
    }

    private func component(_ data: Data) throws -> String {
        guard let value = String(data: data, encoding: .utf8), value.utf8.count <= 128 else {
            throw remoteError(["code": "version_conflict", "message": "The item's base version cannot be safely matched to a remote revision."])
        }
        return value
    }
}

extension Data {
    func hexEncodedString() -> String { map { String(format: "%02x", $0) }.joined() }
}
