import Foundation
import FileProvider
import UniformTypeIdentifiers

private final class ProviderTestEnumerator: NSObject, NSFileProviderEnumerator {
    func invalidate() {}
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) { observer.finishEnumerating(upTo: nil) }
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) { observer.finishEnumeratingChanges(upTo: anchor, moreComing: false) }
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) { completionHandler(nil) }
}
private final class ProviderTestManager: NativeDomainManaging {
    let temporary: URL
    var signals: [NSFileProviderItemIdentifier] = []
    var downloads: [NSFileProviderItemIdentifier] = []
    var evictions: [NSFileProviderItemIdentifier] = []
    var temporaryFailure: Error?
    init(_ temporary: URL) { self.temporary = temporary }
    func temporaryDirectoryURL() throws -> URL { if let failure = temporaryFailure { throw failure }; return temporary }
    func signalEnumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { signals.append(containerItemIdentifier); completionHandler(nil) }
    func signalErrorResolved(_ error: NSError, completionHandler: @escaping (Error?) -> Void) { completionHandler(nil) }
    func requestDownloadForItem(withIdentifier itemIdentifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { downloads.append(itemIdentifier); completionHandler(nil) }
    func evictItem(identifier itemIdentifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { evictions.append(itemIdentifier); completionHandler(nil) }
    func getUserVisibleURL(for itemIdentifier: NSFileProviderItemIdentifier, completionHandler: @escaping (URL?, Error?) -> Void) { completionHandler(temporary, nil) }
    func enumeratorForMaterializedItems() -> NSFileProviderEnumerator { ProviderTestEnumerator() }
}
private final class ProviderTestSystem: NativeSystemManaging {
    let manager: ProviderTestManager
    var failure: Error?
    init(_ manager: ProviderTestManager) { self.manager=manager }
    func domains(completionHandler: @escaping ([NSFileProviderDomain], Error?) -> Void) {
        completionHandler([NSFileProviderDomain(identifier:NSFileProviderDomainIdentifier("budget-domain"),displayName:"Synthetic budget")],failure)
    }
    func manager(for domain: NSFileProviderDomain) -> NativeDomainManaging? { manager }
}
private final class ProviderTestTemplate: NSObject, NSFileProviderItem {
    var type: UTType = .plainText
    var name = "new.txt"
    var tags: Data?
    var attributes: [String: Data] = [:]
    var itemIdentifier: NSFileProviderItemIdentifier { providerID("local-new") }
    var parentItemIdentifier: NSFileProviderItemIdentifier { providerID("new-parent") }
    var filename: String { name }
    var contentType: UTType { type }
    var tagData: Data? { tags }
    var extendedAttributes: [String: Data] { attributes }
}

enum ProviderCoverageTests {
    static func run(_ paths: NativePaths, _ bridge: BackendBridge) throws {
        let temporary = paths.group.appendingPathComponent("provider-delivery")
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let manager = ProviderTestManager(temporary)
        let request = NSFileProviderRequest()
        func provider(_ id: String = "provider-tests", manager selected: ProviderTestManager? = manager, bridge selectedBridge: BackendBridge? = bridge) -> RDriveProvider {
            RDriveProvider(domain: NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(id), displayName: "Synthetic provider"),
                           bridge: selectedBridge, manager: selected, startBackground: false)
        }
        // The production initializer performs only a lookup for a fresh,
        // unregistered synthetic identifier; stop its timer immediately.
        let defaultInstance = RDriveProvider(domain: NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("coverage-" + UUID().uuidString), displayName: "Unregistered coverage fixture"))
        defaultInstance.invalidate()
        let oldDelivery = temporary.appendingPathComponent("rdrive-fetch-old")
        let freshDelivery = temporary.appendingPathComponent("rdrive-fetch-new")
        let unrelated = temporary.appendingPathComponent("unrelated")
        for file in [oldDelivery, freshDelivery, unrelated] { try Data("private fixture".utf8).write(to: file) }
        try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSinceNow: -172800)], ofItemAtPath: oldDelivery.path)
        let background = RDriveProvider(domain: NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("provider-tests"), displayName: "Synthetic provider"), bridge: bridge, manager: manager)
        let cleanupDeadline = Date(timeIntervalSinceNow: 3)
        while FileManager.default.fileExists(atPath: oldDelivery.path) && Date() < cleanupDeadline { Thread.sleep(forTimeInterval: 0.01) }
        background.invalidate()
        try expect(!FileManager.default.fileExists(atPath: oldDelivery.path) && FileManager.default.fileExists(atPath: freshDelivery.path) && FileManager.default.fileExists(atPath: unrelated.path), "Startup cleanup removed a fresh/unowned file or retained an expired owned delivery")
        let active = provider()
        defer { active.invalidate() }
        let version = NSFileProviderItemVersion(contentVersion: Data("content".utf8), metadataVersion: Data("metadata".utf8))
        func item(_ target: RDriveProvider, expectedFailure: Bool = false) throws {
            let done = DispatchSemaphore(value: 0)
            var received: NSFileProviderItem?; var failure: Error?
            _ = target.item(for: providerID("stable-0"), request: request) { received = $0; failure = $1; done.signal() }
            try wait(done)
            try expect((failure != nil) == expectedFailure && (received != nil) != expectedFailure, "Provider item completion was incorrect")
        }
        try item(active)
        try item(provider("provider-error"), expectedFailure: true)
        let unavailable = provider(bridge: nil)
        try item(unavailable, expectedFailure: true)
        try NativeTests.rejects("Provider without bridge produced an enumerator") { _ = try unavailable.enumerator(for: .rootContainer, request: request) }
        try NativeTests.rejects("Remote trash became supported") { _ = try active.enumerator(for: .trashContainer, request: request) }
        let enumerator = try active.enumerator(for: .rootContainer, request: request)
        enumerator.invalidate()

        func fetch(_ target: RDriveProvider, version: NSFileProviderItemVersion? = nil, expectedFailure: Bool = false) throws {
            let done = DispatchSemaphore(value: 0)
            var delivered: URL?; var received: NSFileProviderItem?; var failure: Error?
            let progress = target.fetchContents(for: providerID("stable-0"), version: version, request: request) { delivered = $0; received = $1; failure = $2; done.signal() }
            try wait(done)
            try expect(progress.kind == .file && progress.fileOperationKind == .downloading, "Finder download progress lost its operation kind")
            try expect((failure != nil) == expectedFailure, "Provider fetch completion status was incorrect")
            if !expectedFailure {
                try expect(delivered != nil && received != nil, "Successful native fetch omitted a file/item")
                try expect(try Data(contentsOf: delivered!) == Data("provider-content".utf8), "Provider delivery changed actual bytes")
                try expect(progress.totalUnitCount == Int64(Data("provider-content".utf8).count) && progress.completedUnitCount == progress.totalUnitCount,
                           "Finder download counters were reset by a metadata preflight or finished before delivery")
                try expect(delivered!.deletingLastPathComponent().path == temporary.path, "Delivery escaped the mocked system temporary directory")
                try FileManager.default.removeItem(at: delivered!)
            } else { try expect(delivered == nil && received == nil, "Failed fetch acknowledged a delivery") }
        }
        try fetch(active)
        try fetch(active, version: version)
        for id in ["provider-outside", "provider-link", "provider-directory", "provider-fetch-missing"] { try fetch(provider(id), expectedFailure: true) }
        try fetch(provider(manager: nil), expectedFailure: true)
        let invalidVersion = NSFileProviderItemVersion(contentVersion: Data([255]), metadataVersion: Data())
        try fetch(active, version: invalidVersion, expectedFailure: true)
        manager.temporaryFailure = NativeFailure.invalid("Synthetic temporary directory failure")
        try fetch(active, expectedFailure: true)
        manager.temporaryFailure = nil

        let source = paths.group.appendingPathComponent("provider-source.txt")
        try Data("provider-create".utf8).write(to: source)
        let template = ProviderTestTemplate()
        template.tags = try PropertyListSerialization.data(fromPropertyList: ["Blue\n4"], format: .binary, options: 0)
        template.attributes = ["user.test#S": Data([0, 255]), "security.excluded#S": Data([1])]
        func create(_ target: RDriveProvider = active, fields: NSFileProviderItemFields = [.contents, .tagData, .extendedAttributes], contents: URL? = source,
                    options: NSFileProviderCreateItemOptions = [], expectedFailure: Bool = false) throws {
            let done = DispatchSemaphore(value: 0); var failure: Error?; var pending: NSFileProviderItemFields = []; var received: NSFileProviderItem?
            let progress = target.createItem(basedOn: template, fields: fields, contents: contents, options: options, request: request) {
                received = $0; pending = $1; failure = $3; done.signal()
            }
            try wait(done)
            try expect(progress.kind == .file && progress.fileOperationKind == .uploading, "Finder creation progress lost its upload direction")
            if !expectedFailure && contents != nil {
                try expect(progress.totalUnitCount == Int64(Data("provider-create".utf8).count) && progress.completedUnitCount == progress.totalUnitCount,
                           "Finder creation did not report actual payload bytes")
            }
            try expect((failure != nil) == expectedFailure && (received != nil) != expectedFailure, "Provider create completion status was incorrect")
            try expect(pending == (expectedFailure ? fields : []), "Provider create lost pending field information")
        }
        try create()
        try create(provider("provider-error"), expectedFailure: true)
        template.type = .folder
        try create(contents: nil)
        template.type = .symbolicLink
        try create(contents: nil, expectedFailure: true)
        template.type = .plainText
        try create(contents: nil, options: .mayAlreadyExist, expectedFailure: true)
        template.tags = try PropertyListSerialization.data(fromPropertyList: ["unsupported": 1], format: .binary, options: 0)
        try create(expectedFailure: true)
        template.tags = nil
        try create(fields: [.tagData], contents: nil)

        func modify(_ target: RDriveProvider = active, fields: NSFileProviderItemFields = [.contents, .filename, .parentItemIdentifier, .tagData, .extendedAttributes], contents: URL? = source,
                    base: NSFileProviderItemVersion = version, expectedFailure: Bool = false) throws {
            let done = DispatchSemaphore(value: 0); var failure: Error?; var pending: NSFileProviderItemFields = []; var received: NSFileProviderItem?
            let progress = target.modifyItem(template, baseVersion: base, changedFields: fields, contents: contents, options: [], request: request) {
                received = $0; pending = $1; failure = $3; done.signal()
            }
            try wait(done)
            try expect(progress.kind == .file && progress.fileOperationKind == .uploading, "Finder edit progress lost its upload direction")
            if !expectedFailure && fields.contains(.contents) && contents != nil {
                try expect(progress.totalUnitCount == Int64(Data("provider-create".utf8).count) && progress.completedUnitCount == progress.totalUnitCount,
                           "Finder upload counters were reset by a metadata preflight")
            }
            try expect((failure != nil) == expectedFailure && (received != nil) != expectedFailure, "Provider modify completion status was incorrect")
            try expect(pending == (expectedFailure ? fields : []), "Provider modify lost pending fields")
        }
        try modify()
        try modify(fields: [.tagData], contents: nil)
        try modify(fields: [.contents], contents: nil, expectedFailure: true)
        try modify(base: invalidVersion, expectedFailure: true)
        try modify(base: NSFileProviderItemVersion(contentVersion: Data(), metadataVersion: Data(repeating: 1, count: 129)), expectedFailure: true)
        try modify(provider("provider-error"), expectedFailure: true)
        try expect(manager.signals.contains(.workingSet) && manager.signals.contains(providerID("new-parent")) && manager.signals.contains(.rootContainer), "Provider changes did not signal current/previous parents")
        for target in [active, provider("provider-error")] {
            let done = DispatchSemaphore(value: 0); var failure: Error?
            _ = target.deleteItem(identifier: providerID("stable-0"), baseVersion: version, options: .recursive, request: request) { failure = $0; done.signal() }
            try wait(done)
            try expect((failure != nil) == (target !== active), "Provider delete response status changed")
        }
        let rejectedPin = provider("provider-pin-error")
        let pinDone = DispatchSemaphore(value: 0); var pinFailure: Error?
        _ = rejectedPin.performAction(identifier: NSFileProviderExtensionActionIdentifier("rdrive.pin"), onItemsWithIdentifiers: [providerID("stable-0")]) { pinFailure = $0; pinDone.signal() }
        try wait(pinDone); try expect(pinFailure != nil, "Failed pin was acknowledged")
        rejectedPin.invalidate()
        for action in ["rdrive.pin", "rdrive.unpin"] {
            let done = DispatchSemaphore(value: 0); var failure: Error?
            _ = active.performAction(identifier: NSFileProviderExtensionActionIdentifier(action), onItemsWithIdentifiers: [providerID("stable-0")]) { failure = $0; done.signal() }
            try wait(done)
            try expect(failure == nil, "Provider pin policy action failed")
        }
        let signalsBeforeUnsupportedActions = manager.signals
        for action in ["rdrive.download", "rdrive.evict", "unsupported"] {
            let done = DispatchSemaphore(value: 0)
            var failure: NSError?
            _ = active.performAction(identifier: NSFileProviderExtensionActionIdentifier(action), onItemsWithIdentifiers: [providerID("stable-0")]) {
                failure = $0 as NSError?
                done.signal()
            }
            try wait(done)
            try expect(failure?.domain == NSCocoaErrorDomain && failure?.code == NSFeatureUnsupportedError,
                       "A stale or unknown custom action did not return the unsupported-feature error")
        }
        try expect(manager.downloads.isEmpty && manager.evictions.isEmpty && manager.signals == signalsBeforeUnsupportedActions,
                   "A stale or unknown custom action entered the system manager")
        func refresh(_ target: RDriveProvider, identifiers: [NSFileProviderItemIdentifier], failureCode: Int? = nil) throws {
            let done = DispatchSemaphore(value: 0)
            var failure: NSError?
            _ = target.performAction(identifier: NSFileProviderExtensionActionIdentifier("rdrive.refresh"), onItemsWithIdentifiers: identifiers) {
                failure = $0 as NSError?
                done.signal()
            }
            try wait(done)
            try expect(failure?.code == failureCode, "Refresh completion did not reflect the bridge response or selection")
        }
        try refresh(active, identifiers: [providerID("folder-one")])
        try refresh(active, identifiers: [.rootContainer])
        try refresh(active, identifiers: [.workingSet])
        try refresh(active, identifiers: [], failureCode: NSFeatureUnsupportedError)
        try refresh(active, identifiers: [providerID("folder-one"), .rootContainer], failureCode: NSFeatureUnsupportedError)
        try refresh(active, identifiers: [providerID("stable-0")], failureCode: NSFeatureUnsupportedError)
        let missingRefreshAcknowledgement = provider("provider-refresh-no-ack")
        defer { missingRefreshAcknowledgement.invalidate() }
        try refresh(missingRefreshAcknowledgement, identifiers: [.rootContainer], failureCode: NSFileProviderError.serverUnreachable.rawValue)
        try expect(manager.downloads.isEmpty && manager.evictions.isEmpty && manager.signals == signalsBeforeUnsupportedActions,
                   "Refresh entered the system manager")
        let cancelled = provider("provider-cancel")
        let cancelDone = DispatchSemaphore(value: 0); var cancelError: Error?; var callbacks = 0
        let progress = cancelled.item(for: providerID("stable-0"), request: request) { _, error in cancelError = error; callbacks += 1; cancelDone.signal() }
        progress.cancel()
        try wait(cancelDone)
        try expect((cancelError as NSError?)?.code == NSUserCancelledError && callbacks == 1, "Cancelled provider callback was not completed exactly once")
        cancelled.invalidate()
        let closed = provider()
        closed.invalidate()
        try item(closed, expectedFailure: true)
        try smartBudgetCallbacks(paths)
        print("PASS provider SDK callbacks through synthetic IPC, byte delivery, revision/field guards, injected actions and cancellation")
    }
    private static func smartBudgetCallbacks(_ paths: NativePaths) throws {
        let peer=try BudgetPeer(root:paths.group);defer{peer.stop()}
        let bridge=BackendBridge(paths:peer.paths)
        let destination=peer.paths.group.appendingPathComponent("delivered")
        try FileManager.default.createDirectory(at:destination,withIntermediateDirectories:false)
        let manager=ProviderTestManager(destination)
        let system=ProviderTestSystem(manager)
        let domain=NSFileProviderDomain(identifier:NSFileProviderDomainIdentifier("budget-domain"),displayName:"Synthetic budget")
        let item:[String:Any]=["id":"smart","parent_id":"root","name":"smart.txt","kind":"file","size":16,"content_version":"c","metadata_version":"m","pinned":false,"capabilities":["read"],"smart":true,"has_unsynced_changes":false]
        let fetchGate=DispatchSemaphore(value:0)
        defer{fetchGate.signal()}
        peer.mutate{$0.items["smart"]=item;$0.fetchGate=fetchGate}
        let provider=RDriveProvider(domain:domain,bridge:bridge,manager:manager,startBackground:false,heartbeatInterval:0.01,budgetFactory:{NativeMaterializationBudget(bridge:$0,domain:$1,system:system,timeout:0.1)})
        defer{provider.invalidate()}
        let state=peer.paths.group.appendingPathComponent("native-budget.json")
        func reservations()->[String:[String:Any]] {
            guard let bytes=try? Data(contentsOf:state),let raw=try? JSONSerialization.jsonObject(with:bytes) as? [String:Any] else{return [:]}
            return raw["reservations"] as? [String:[String:Any]] ?? [:]
        }
        func releaseAll() {NativeMaterializationBudget(bridge:bridge,domain:"budget-domain",system:system).release(Array(reservations().keys))}
        func observedRenewal(_ before:[String:[String:Any]],by deadline:DispatchTime)->Bool {
            guard peer.budgetRenewed.wait(timeout:deadline) == .success else{return false}
            while DispatchTime.now() < deadline {
                let current=reservations()
                if before.count==1,current.count==1,let (key,previous)=before.first,
                   let updated=current[key],updated["delivered"] as? Bool==false,
                   let oldExpiry=previous["expires"] as? Double,let newExpiry=updated["expires"] as? Double,
                   newExpiry>oldExpiry,peer.inspect({$0.ledger["budget-domain"]?["reserved"]})==16 {return true}
                Thread.sleep(forTimeInterval:0.002)
            }
            return false
        }
        let success=DispatchSemaphore(value:0);var delivered:URL?;var error:Error?
        _=provider.fetchContents(for:providerID("smart"),version:nil,request:NSFileProviderRequest()){delivered=$0;error=$2;success.signal()}
        try wait(peer.fetchStarted)
        let beforeRenewal=reservations()
        try expect(observedRenewal(beforeRenewal,by:.now()+5),"Admission heartbeat did not publish and durably extend the undelivered reservation while fetch was gated")
        try expect(success.wait(timeout:.now()) == .timedOut,"Smart fetch completed before its content gate was released")
        fetchGate.signal()
        try wait(success);try wait(peer.fetchFinished)
        peer.mutate{$0.fetchGate=nil}
        try expect(error==nil && delivered != nil && Data(contentsOf:delivered!)==Data("provider-content".utf8),"Smart callback did not deliver actual admitted bytes")
        try expect(reservations().count==1 && reservations().values.first?["delivered"] as? Bool==true,"Smart callback did not retain its delivered admission receipt")
        try FileManager.default.removeItem(at:delivered!);releaseAll()
        try expect(reservations().isEmpty,"Discarded delivered lease was not released")
        // Negative control: the same probe rejects a provider whose heartbeat
        // cannot fire within the observation window; delivery stays gated.
        let disabledGate=DispatchSemaphore(value:0)
        defer{disabledGate.signal()}
        peer.mutate{$0.fetchGate=disabledGate}
        while peer.budgetRenewed.wait(timeout:.now()) == .success {}
        while peer.fetchStarted.wait(timeout:.now()) == .success {}
        let disabledProvider=RDriveProvider(domain:domain,bridge:bridge,manager:manager,startBackground:false,heartbeatInterval:3600,budgetFactory:{NativeMaterializationBudget(bridge:$0,domain:$1,system:system,timeout:0.1)})
        defer{disabledProvider.invalidate()}
        let disabledDone=DispatchSemaphore(value:0);var disabledDelivery:URL?;var disabledError:Error?
        _=disabledProvider.fetchContents(for:providerID("smart"),version:nil,request:NSFileProviderRequest()){disabledDelivery=$0;disabledError=$2;disabledDone.signal()}
        try wait(peer.fetchStarted)
        try expect(!observedRenewal(reservations(),by:.now()+0.1),"Heartbeat probe accepted a provider with renewals disabled")
        try expect(disabledDone.wait(timeout:.now()) == .timedOut,"Negative-control content escaped its gate")
        disabledGate.signal();try wait(disabledDone);try wait(peer.fetchFinished)
        try expect(disabledError==nil && disabledDelivery != nil,"Negative-control fetch failed after its gate was released")
        try FileManager.default.removeItem(at:disabledDelivery!);releaseAll()
        disabledProvider.invalidate()
        peer.mutate{$0.fetchFailure=true;$0.fetchGate=nil}
        let failed=DispatchSemaphore(value:0)
        _=provider.fetchContents(for:providerID("smart"),version:nil,request:NSFileProviderRequest()){_,_,failure in error=failure;failed.signal()}
        try wait(failed);try wait(peer.fetchFinished)
        try expect(error != nil && reservations().isEmpty,"Failed Smart fetch retained a download admission")
        peer.mutate{$0.fetchFailure=false;$0.limit=1}
        let rejected=DispatchSemaphore(value:0)
        _=provider.fetchContents(for:providerID("smart"),version:nil,request:NSFileProviderRequest()){_,_,failure in error=failure;rejected.signal()}
        try wait(rejected)
        try expect((error as NSError?)?.code==NSFileProviderError.insufficientQuota.rawValue && peer.inspect{$0.calls.contains{$0["op"] as? String=="native_error"}},"Quota rejection did not preserve a visible provider error")
        peer.mutate{$0.limit=1024;$0.fetchWaitForCancellation=true}
        while peer.fetchStarted.wait(timeout:.now()) == .success {}
        let cancellingProvider=RDriveProvider(domain:domain,bridge:bridge,manager:manager,startBackground:false,heartbeatInterval:0.2,budgetFactory:{NativeMaterializationBudget(bridge:$0,domain:$1,system:system,timeout:0.1)})
        defer{cancellingProvider.invalidate()}
        let cancelled=DispatchSemaphore(value:0)
        _=cancellingProvider.fetchContents(for:providerID("smart"),version:nil,request:NSFileProviderRequest()){_,_,failure in error=failure;cancelled.signal()}
        try wait(peer.fetchStarted)
        try expect(!reservations().isEmpty,"Smart content request began without its durable admission")
        try FileManager.default.setAttributes([.posixPermissions:0o644],ofItemAtPath:state.path)
        try wait(cancelled);try wait(peer.fetchFinished)
        try expect((error as NSError?)?.code==NSUserCancelledError,"Lost admission heartbeat did not cancel delivery: \(String(describing:error))")
        try FileManager.default.setAttributes([.posixPermissions:0o600],ofItemAtPath:state.path)
        releaseAll()
        peer.mutate{$0.fetchWaitForCancellation=false}
        let before=peer.inspect{$0.calls.count}
        let background=RDriveProvider(domain:domain,bridge:bridge,manager:manager,backgroundInterval:0.01,budgetFactory:{NativeMaterializationBudget(bridge:$0,domain:$1,system:system,timeout:0.1)})
        var callbacks=0
        background.materializedItemsDidChange{callbacks += 1}
        try UICoverageTests.pump("Background materialized-set sweep did not reach the private peer") {peer.inspect{$0.calls.count}>before+3}
        system.failure=NativeFailure.bridge("Synthetic native scan failure")
        background.materializedItemsDidChange{callbacks += 1}
        RunLoop.current.run(until:Date(timeIntervalSinceNow:0.05))
        background.invalidate()
        background.materializedItemsDidChange{callbacks += 1}
        try expect(callbacks==3,"Materialized-set callbacks were not completed while stopped/in flight")
        system.failure=nil
    }

}
