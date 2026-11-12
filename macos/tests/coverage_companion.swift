import Foundation
import AppKit
import FileProvider
import Darwin
@testable import RDriveCompanionCoverage

private typealias CompanionPaths = RDriveCompanionCoverage.NativePaths
private typealias CompanionBridge = RDriveCompanionCoverage.BackendBridge
private let companionTestError = NSError(domain: "SyntheticCompanion", code: 17)

private final class CompanionTestEnumerator: NSObject, NSFileProviderEnumerator {
    func invalidate() {}
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) { observer.finishEnumerating(upTo: nil) }
}
private final class CompanionTestManager: RDriveCompanionCoverage.NativeDomainManaging {
    var url: URL?
    var error: Error?
    var signals: [String] = []
    var downloads: [String] = []
    var evictions: [String] = []
    var resolveVisible = true
    var invisibleIDs = Set<String>()
    var resolvedErrors: [NSError] = []
    var resolutionError: Error?
    var holdResolution = false
    var resolutionReply: ((Error?) -> Void)?
    init(_ url: URL) { self.url = url }
    func enumeratorForMaterializedItems() -> NSFileProviderEnumerator { CompanionTestEnumerator() }
    func getUserVisibleURL(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (URL?, Error?) -> Void) { if resolveVisible { completionHandler(invisibleIDs.contains(identifier.rawValue) ? nil : url,error) } }
    func evictItem(identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { evictions.append(identifier.rawValue);completionHandler(error) }
    func signalEnumerator(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { signals.append(identifier.rawValue);completionHandler(error) }
    func signalErrorResolved(_ error: NSError, completionHandler: @escaping (Error?) -> Void) {
        resolvedErrors.append(error)
        if holdResolution { resolutionReply = completionHandler } else { completionHandler(resolutionError) }
    }
    func requestDownloadForItem(withIdentifier identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (Error?) -> Void) { downloads.append(identifier.rawValue);completionHandler(error) }
    func temporaryDirectoryURL() throws -> URL { if let error=error {throw error};return url! }
}
private final class CompanionTestSystem: RDriveCompanionCoverage.NativeSystemManaging {
    var known: [NSFileProviderDomain] = []
    var error: Error?
    var domainCalls = 0
    var domainsSilent = false
    var managerDomains: [String] = []
    var unavailableDomains = Set<String>()
    var selected: CompanionTestManager?
    func domains(completionHandler: @escaping ([NSFileProviderDomain], Error?) -> Void) { domainCalls += 1;if !domainsSilent { completionHandler(known,error) } }
    func manager(for domain: NSFileProviderDomain) -> RDriveCompanionCoverage.NativeDomainManaging? { managerDomains.append(domain.identifier.rawValue);return unavailableDomains.contains(domain.identifier.rawValue) ? nil : selected }
}
private final class CompanionTestProcess: RDriveCompanionCoverage.CompanionProcessManaging {
    var isRunning = true
    var processIdentifier: Int32 = 12345
    var arguments: [String] = []
    var failure: Error?
    var launches = 0
    func launch(executable: URL?, arguments: [String]) throws { launches += 1;self.arguments=arguments;if let failure=failure {throw failure} }
}

enum CompanionCoverageTests {
    static func run(_ localPaths: NativePaths, _ localBridge: BackendBridge) throws {
        let paths=CompanionPaths(group:localPaths.group,development:localPaths.development)
        let bridge=CompanionBridge(paths:paths)
        let system=CompanionTestSystem()
        let manager=CompanionTestManager(paths.group)
        system.selected=manager
        system.known=[NSFileProviderDomain(identifier:NSFileProviderDomainIdentifier("synthetic"),displayName:"Synthetic")]
        var integration=RDriveCompanionCoverage.CompanionControlIntegration.live
        integration.paths={ _ in paths };integration.system=system
        var added:[String]=[];var removed:[String]=[];var enabled=0;var watched=0;var tags:[String]=[]
        var callbackError:Error?
        integration.addDomain={domain,done in added.append(domain.identifier.rawValue);done(callbackError)}
        integration.removeDomain={domain,done in removed.append(domain.identifier.rawValue);done(paths.group,callbackError)}
        integration.identify={_,done in done(NSFileProviderItemIdentifier("canonical"),NSFileProviderDomainIdentifier("synthetic"),callbackError)}
        integration.finderEnabled={true};integration.enableFinder={enabled += 1}
        integration.setTags={_,values in if let error=callbackError {throw error};tags=values}
        integration.budget={RDriveCompanionCoverage.NativeMaterializationBudget(bridge:$0,domain:$1,system:system)}
        integration.watch={_ in watched += 1;return 0}
        func control(_ values:[String])->Int32? {RDriveCompanionCoverage.controlCommand(arguments:["Companion"]+values,integration:integration)}
        try expect(RDriveCompanionCoverage.option("--one",arguments:["app","--one","value"])=="value", "Control option parsing lost its following value")
        try expect(RDriveCompanionCoverage.option("--missing",arguments:["app"])==nil && RDriveCompanionCoverage.option("--last",arguments:["app","--last"])==nil,"Missing option values were manufactured")
        try expect(RDriveCompanionCoverage.runUntil { $0(nil) }==0,"Immediate control completion failed")
        try expect(RDriveCompanionCoverage.runUntil { $0(companionTestError) }==1,"Control failure was acknowledged")
        try expect(RDriveCompanionCoverage.runUntil(timeout:0) {_ in}==1,"Silent control operation did not time out")
        try expect(control(["--diagnose"])==0 && control(["--list-domains"])==0,"Read-only controls failed")
        try expect(control(["--register-domain","synthetic","--display-name","Named"])==0,"Injected registration failed")
        try expect(control(["--remove-domain","synthetic"])==0 && added==["synthetic"] && removed==["synthetic"],"Registration/removal did not use the injected boundary")
        try expect(control(["--domain-root","synthetic"])==0 && control(["--resolve-file",paths.group.path])==0,"Identity controls failed")
        try expect(control(["--set-tags",paths.group.path,"Blue","Custom"])==0 && tags==["Blue","Custom"],"Tag arguments changed")
        try expect(control(["--enable-finder"])==0 && enabled==1,"Finder enable did not use the injected boundary")
        try expect(control(["--watch-domains"])==0 && watched==1,"Watcher did not use the injected boundary")
        try expect(control(["--trim-cache","0"])==0,"Synthetic zero-byte trim failed")
        for action in ["--pin","--unpin","--check-cache"] {
            try expect(control([action,"synthetic","stable-0"])==0,"Synthetic policy/preflight failed")
            try expect(control([action,"synthetic"])==2,"Missing policy item was accepted")
        }
        for action in ["--signal","--download","--evict"] {try expect(control([action,"synthetic","canonical"])==0,"Injected system action failed")}
        try expect(manager.downloads==["canonical"] && manager.evictions==["canonical"] && !manager.signals.isEmpty,"System actions bypassed the injected manager")
        try expect(control(["--download","synthetic"])==1,"Missing download item was accepted")
        try expect(control(["--trim-cache","invalid"])==2 && control(["--unknown"])==2,"Invalid command parameters were accepted")
        try expect(control([])==nil && control(["--show-message","text"])==nil,"GUI arguments were consumed as control commands")
        callbackError=companionTestError
        for args in [["--register-domain","synthetic"],["--remove-domain","synthetic"],["--resolve-file",paths.group.path],["--set-tags",paths.group.path,"Blue"]] {try expect(control(args)==1,"Injected control error was acknowledged")}
        system.error=companionTestError;try expect(control(["--list-domains"])==1,"Domain-list failure ignored");system.error=nil
        manager.error=companionTestError;try expect(control(["--domain-root","synthetic"])==1 && control(["--download","synthetic","canonical"])==1,"Manager failure ignored");manager.error=nil
        system.selected=nil;try expect(control(["--domain-root","synthetic"])==1 && control(["--signal","synthetic"])==1,"Missing manager ignored");system.selected=manager
        integration.paths={_ in throw companionTestError}
        try expect(control(["--diagnose"])==0,"Diagnostics lost configuration failure details")
        for args in [["--watch-domains"],["--trim-cache","0"],["--pin","synthetic","stable-0"]] {try expect(control(args)==1,"Private path failure was ignored")}
        integration.paths={_ in paths}
        try configuredPathTests(paths)
        try retrySyncTests(manager, system, integration)
        try transferProgressTests(paths, bridge, system)
        try watcherTests(bridge,system)
        try hostTests(paths,bridge,system)
        try ownedSignalAndTagBoundaries(paths)
        print("PASS injectable Companion parsing, callback acknowledgement, private paths, watcher lifecycle and host ownership without OS registration/UI")
    }

    private static func retrySyncTests(_ manager: CompanionTestManager, _ system: CompanionTestSystem,
                                       _ integration: RDriveCompanionCoverage.CompanionControlIntegration) throws {
        let codes: [NSFileProviderError.Code] = [.serverUnreachable, .cannotSynchronize]
        var completions = 0
        var failure: Error?
        manager.resolvedErrors.removeAll(); manager.holdResolution = true
        RDriveCompanionCoverage.signalNativeSyncErrorsResolved(manager, codes: codes) { failure = $0; completions += 1 }
        try expect(completions == 0 && manager.resolvedErrors.count == 1, "Error resolution was acknowledged before its first OS callback")
        let first = manager.resolutionReply; manager.resolutionReply = nil; first?(nil)
        try expect(completions == 0 && manager.resolvedErrors.count == 2, "Error resolution skipped the second OS callback")
        let second = manager.resolutionReply; manager.resolutionReply = nil; second?(nil)
        try expect(completions == 1 && failure == nil && manager.resolvedErrors.map(\.code) == codes.map(\.rawValue)
                   && manager.resolvedErrors.allSatisfy({ $0.domain == NSFileProviderErrorDomain }), "Resolved error codes lost order, domain or completion")
        manager.holdResolution = false
        manager.resolutionError = companionTestError; manager.resolvedErrors.removeAll(); completions = 0
        RDriveCompanionCoverage.signalNativeSyncErrorsResolved(manager, codes: codes) { failure = $0; completions += 1 }
        try expect(completions == 1 && (failure as NSError?) === companionTestError && manager.resolvedErrors.count == 1,
                   "Failed resolution was hidden or later errors were signalled after failure")
        manager.resolutionError = nil; manager.resolvedErrors.removeAll()
        func control(_ values: [String]) -> Int32? {
            RDriveCompanionCoverage.controlCommand(arguments: ["Companion"] + values, integration: integration)
        }
        try expect(control(["--retry-sync", "synthetic"]) == 0 && manager.resolvedErrors.map(\.code) == codes.map(\.rawValue),
                   "Retry-sync did not reschedule only the known resolved errors")
        for args in [["--retry-sync"], ["--retry-sync", ""], ["--retry-sync", "--diagnose"]] {
            try expect(control(args) == 2, "Retry-sync accepted a missing domain")
        }
        try expect(control(["--retry-sync", "not-registered"]) == 1, "Retry-sync sent notifications to an unregistered domain")
        system.error = companionTestError
        try expect(control(["--retry-sync", "synthetic"]) == 1, "Retry-sync ignored a domain-list error")
        system.error = nil; system.selected = nil
        try expect(control(["--retry-sync", "synthetic"]) == 1, "Retry-sync ignored an unavailable manager")
        system.selected = manager; manager.resolutionError = companionTestError; manager.resolvedErrors.removeAll()
        try expect(control(["--retry-sync", "synthetic"]) == 1 && manager.resolvedErrors.count == 1,
                   "Retry-sync reported success before both resolution callbacks")
        manager.resolutionError = nil
    }

    private static func transferProgressTests(_ paths: CompanionPaths, _ bridge: CompanionBridge, _ system: CompanionTestSystem) throws {
        let fixture = paths.group.appendingPathComponent("transfer-progress-fixture.json")
        let file = paths.group.appendingPathComponent("progress-owned-file.txt")
        try Data("owned progress fixture".utf8).write(to: file)
        let manager = CompanionTestManager(file)
        let previous = system.selected
        let previousDomains = system.known
        system.selected = manager
        system.known = [NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("progress-domain"), displayName: "Progress fixture")]
        defer { system.selected = previous; system.known = previousDomains; try? FileManager.default.removeItem(at: fixture) }
        let domain: [String: Any] = ["id": "progress-domain", "display_name": "Progress fixture", "smart": true]
        func row(_ changes: [String: Any] = [:]) -> [String: Any] {
            var value: [String: Any] = ["transfer_id": "attempt", "id": "stable-0", "operation": "upload", "completed": 2,
                "total": 10, "bytes_per_second": 4, "eta_seconds": 2, "state": "transferring", "background": true]
            value.merge(changes) { _, next in next }; return value
        }
        func install(_ rows: [[String: Any]], domains: [[String: Any]]? = nil, reply: [String: Any]? = nil) throws {
            var value: [String: Any] = ["domains": domains ?? [domain], "transfers": rows]
            if let reply = reply { value["reply"] = reply }
            try JSONSerialization.data(withJSONObject: value).write(to: fixture, options: .atomic)
        }
        var started: [Progress] = []; var ended: [Progress] = []
        let watcher = RDriveCompanionCoverage.NativeBackgroundTransferWatcher(bridge: bridge, system: system, timeout: 0.02,
            publish: { started.append($0) }, unpublish: { ended.append($0) })
        func poll(_ selected: RDriveCompanionCoverage.NativeBackgroundTransferWatcher = watcher, fails: Bool = false) throws {
            let done = DispatchSemaphore(value: 0); var failure: Error?
            selected.poll { failure = $0; done.signal() }; try wait(done)
            try expect((failure != nil) == fails, "Background progress poll returned the wrong status")
        }
        func stop(_ selected: RDriveCompanionCoverage.NativeBackgroundTransferWatcher) throws {
            let done = DispatchSemaphore(value: 0); selected.stop { done.signal() }; try wait(done)
        }
        try install([row(["background": false]), row(["state": "completed"])])
        try poll(); try expect(started.isEmpty, "Foreground or already-finished transfers produced duplicate Finder progress")
        try install([row()]); try poll()
        let uploading = started.last!
        try expect(started.count == 1 && uploading.kind == .file && uploading.fileOperationKind == .uploading && uploading.fileURL == file,
                   "Background upload progress lost the authoritative manager URL or direction")
        try expect(!uploading.isCancellable && !uploading.isPausable && uploading.completedUnitCount == 2 && uploading.totalUnitCount == 10,
                   "Background upload exposed unsupported controls or invented counters")
        try install([row()], domains: [domain, ["id": "unregistered-progress-domain", "smart": true]])
        try poll()
        try expect(started.count == 1 && ended.isEmpty && !system.managerDomains.contains("unregistered-progress-domain"),
                   "An unrelated unregistered domain suppressed a healthy transfer")
        manager.invisibleIDs.insert("not-yet-visible")
        try install([row(["transfer_id": "unresolved-create", "id": "not-yet-visible"]), row()]); try poll()
        try expect(started.count == 1 && ended.isEmpty, "One unresolved create hid another item's healthy upload progress")
        manager.invisibleIDs.removeAll()
        system.known.append(NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("unavailable-domain"), displayName: "Unavailable fixture"))
        system.unavailableDomains.insert("unavailable-domain")
        try install([row()], domains: [domain, ["id": "unavailable-domain", "smart": true]]); try poll()
        try expect(started.count == 1 && ended.isEmpty, "An unavailable domain suppressed another domain's healthy transfer")
        system.unavailableDomains.removeAll()
        try install([row(["state": "committing", "completed": 10])]); try poll()
        try expect(started.count == 1 && uploading.completedUnitCount == 9 && !uploading.isFinished,
                   "Unconfirmed remote publication was reported as complete")
        try install([row(["state": "completed", "completed": 10])]); try poll(); try poll()
        try expect(ended.count == 1 && uploading.isFinished && started.count == 1, "Receipt completion leaked progress or restarted an old terminal transfer")
        try install([row(["transfer_id": "download", "operation": "download", "total": NSNull(), "eta_seconds": NSNull(), "completed": 3])]); try poll()
        let downloading = started.last!
        try expect(downloading.fileOperationKind == .downloading && downloading.isIndeterminate && downloading.completedUnitCount == 3,
                   "Unknown-length download displayed a fabricated fraction")
        try install([row(["transfer_id": "download", "state": "failed"]) ]); try poll()
        try expect(!downloading.isFinished && ended.count == 2, "Failed transfer was marked successfully complete")
        try install([row(["transfer_id": "retry"])]); try poll()
        let beforeReplacement = started.last!
        try install([row(["transfer_id": "retry", "id": "replacement", "operation": "download"])]); try poll()
        try expect(started.count == 4 && ended.contains(where: { $0 === beforeReplacement }) && !beforeReplacement.isFinished,
                   "Reused transfer identity retained progress for another file")
        let cancelled = started.last!
        try install([row(["transfer_id": "retry", "state": "cancelled"])]); try poll()
        try expect(ended.contains(where: { $0 === cancelled }) && !cancelled.isFinished, "Cancelled transfer was reported as successful")
        try install([row()]); try poll()
        let missing = started.last!
        try install([]); try poll()
        try expect(ended.contains(where: { $0 === missing }) && !missing.isFinished, "A missing transfer snapshot left stale progress or fabricated success")
        try install([row()]); try poll()
        let invalidated = started.last!
        try install([row(["state": "unknown"])]); try poll(fails: true)
        try expect(ended.contains(where: { $0 === invalidated }) && !invalidated.isFinished, "Invalid status retained stale progress")
        try install([], reply: ["ok": true]); try poll(fails: true)
        try install([], domains: [["smart": true]]); try poll(fails: true)
        try install([row()]); system.selected = nil; try poll(); system.selected = manager
        system.error = companionTestError; try poll(fails: true); system.error = nil
        system.domainsSilent = true; try poll(fails: true); system.domainsSilent = false
        manager.error = companionTestError; try poll(); manager.error = nil
        manager.url = nil; try poll()
        manager.url = URL(string: "https://example.invalid/file"); try poll(); manager.url = file
        manager.resolveVisible = false; try poll(); manager.resolveVisible = true
        try install(Array(repeating: row(["background": false]), count: 257)); try poll(fails: true)
        system.known.append(NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("second-domain"), displayName: "Second progress fixture"))
        try install((0..<256).map { row(["transfer_id": "bounded-\($0)"]) }, domains: [domain, ["id": "second-domain", "smart": true]])
        try poll(fails: true)
        try install([row()]); manager.resolveVisible = false
        let pending = DispatchSemaphore(value: 0)
        watcher.poll { _ in pending.signal() }
        let busy = DispatchSemaphore(value: 0); watcher.poll { _ in busy.signal() }; try wait(busy)
        try stop(watcher); try wait(pending); manager.resolveVisible = true
        try poll()
        // The real Foundation publish/unpublish boundary uses only our own
        // temporary file and a mocked manager, never a registered cloud item.
        let real = RDriveCompanionCoverage.NativeBackgroundTransferWatcher(bridge: bridge, system: system)
        try install([row()]); try poll(real)
        try install([row(["state": "completed", "completed": 10])]); try poll(real)
        try stop(real)
        try expect(ended.count == started.count, "Background transfer publication leaked an owned progress object")
    }

    private static func configuredPathTests(_ paths:CompanionPaths) throws {
        try expect(try RDriveCompanionCoverage.configuredPaths(arguments:["app","--group-directory",paths.group.path]).group.standardizedFileURL.path==paths.group.standardizedFileURL.path,"Control private path changed")
        try NativeTests.rejects("Relative control group accepted") {_=try RDriveCompanionCoverage.configuredPaths(arguments:["app","--group-directory","relative"])}
        let publicGroup=paths.group.appendingPathComponent("companion-public")
        try FileManager.default.createDirectory(at:publicGroup,withIntermediateDirectories:true,attributes:[.posixPermissions:0o755])
        try NativeTests.rejects("Public control group accepted") {_=try RDriveCompanionCoverage.configuredPaths(arguments:["app","--group-directory",publicGroup.path])}
        let bundleURL=paths.group.appendingPathComponent(UUID().uuidString+".bundle")
        let contents=bundleURL.appendingPathComponent("Contents")
        try FileManager.default.createDirectory(at:contents,withIntermediateDirectories:true)
        let newGroup=paths.group.appendingPathComponent("companion-created-"+UUID().uuidString)
        try PropertyListSerialization.data(fromPropertyList:["RDriveDevelopmentIPCRoot":newGroup.path,"CFBundleIdentifier":UUID().uuidString],format:.xml,options:0).write(to:contents.appendingPathComponent("Info.plist"))
        try expect(try RDriveCompanionCoverage.configuredPaths(arguments:["app"],bundle:Bundle(url:bundleURL)!).group.path==newGroup.standardizedFileURL.path,"Declared development group was not created privately")
    }

    private static func watcherTests(_ bridge:CompanionBridge,_ system:CompanionTestSystem) throws {
        var stop:(()->Void)?;var ticks=0;var waits=0
        let previous = system.domainCalls
        let runtime=RDriveCompanionCoverage.CompanionWatchIntegration(signals:{stop=$0;return []},timer:{tick in ticks += 1;return Timer(timeInterval:3,repeats:false){_ in tick()}},wait:{waits += 1;let deadline=Date(timeIntervalSinceNow:1);while system.domainCalls==previous && Date()<deadline {RunLoop.current.run(until:Date(timeIntervalSinceNow:0.01))};stop?()})
        try expect(RDriveCompanionCoverage.watchDomains(bridge,system:system,integration:runtime)==0 && ticks==1 && waits==1,"Watcher did not stop/clean up its own scoped lifecycle")
    }

    private static func hostTests(_ paths:CompanionPaths,_ bridge:CompanionBridge,_ system:CompanionTestSystem) throws {
        var messages:[String]=[];var timers:[()->Void]=[];var interrupted:[Int32]=[];var enabled=0;var opened=0;var quit=0;var registrations=0
        var rows:[[String:Any]]=[["id":"host","display_name":"Host","local_root":paths.group.path,"smart":true],["display_name":"Ignored"],["id":"no-root"]]
        var requestError:Error?;var status:[String:Any]=["state":"cached","message":"Synthetic detail"]
        var refreshResponse: [String: Any] = ["queued": true, "completed": false]
        let process=CompanionTestProcess()
        var environment=RDriveCompanionCoverage.CompanionHostIntegration.live
        environment.arguments=["app","--show-status","host","file","--show-message","Launch note"]
        environment.paths={paths};environment.bridge={_ in bridge};environment.watcher={RDriveCompanionCoverage.NativeDomainWatcher(bridge:$0,system:system)}
        environment.request={_,request in
            if let error=requestError {throw error}
            if request["op"] as? String == "refresh_index" {
                try expect(request["domain"] as? String == "host" && request["id"] as? String == "root",
                           "Companion refresh changed the domain or root scope")
                try expect(request["recursive"] as? Bool == true, "Connection refresh did not queue a complete index walk")
                return refreshResponse
            }
            return request["op"] as? String=="status" ? status:["domains":rows]
        }
        environment.configureStatus={_ in nil};environment.process={process};environment.serviceURL={paths.group.appendingPathComponent("synthetic-rdrive")};environment.home={paths.group.path}
        environment.interrupt={interrupted.append($0)};environment.timer={_,tick in timers.append(tick);return Timer(timeInterval:60,repeats:false){_ in tick()}}
        environment.after={DispatchQueue.main.async(execute:$0)};environment.addDomain={_,done in registrations += 1;done(nil)}
        environment.message={messages.append($0)};environment.enableFinder={enabled += 1};environment.openStorage={opened += 1};environment.quit={quit += 1}
        let host=RDriveCompanionCoverage.CompanionDelegate(integration:environment)
        let launched=Notification(name:NSApplication.didFinishLaunchingNotification)
        let terminated=Notification(name:NSApplication.willTerminateNotification)
        host.applicationDidFinishLaunching(launched)
        try UICoverageTests.pump("Companion did not build its detached connection menu") {host.menu.items.dropFirst().first?.submenu?.items.contains(where:{$0.title=="Host"})==true && messages.contains("Launch note") && messages.contains("Cached\n\nSynthetic detail")}
        try UICoverageTests.pump("Companion did not write its private mapped roots") {(try? JSONDecoder().decode([RDriveCompanionCoverage.WatchedRoot].self,from:Data(contentsOf:paths.roots)))?.first?.domain=="host"}
        let mapped=try JSONDecoder().decode([RDriveCompanionCoverage.WatchedRoot].self,from:Data(contentsOf:paths.roots))
        try expect(mapped.count==1 && mapped[0].domain=="host","Companion mapped unconfigured/unscoped roots")
        func refreshMenu() -> NSMenu? {
            host.menu.items.first(where: { $0.title == "Refresh Connection Index" })?.submenu
        }
        try expect(refreshMenu()?.items.map(\.title) == ["Host"], "Refresh Connection offered a non-Smart connection")
        let refreshItem = refreshMenu()!.items[0]
        host.refreshConnection(NSMenuItem())
        host.refreshConnection(refreshItem)
        try UICoverageTests.pump("Companion did not acknowledge the queued root refresh") { messages.contains("Connection index refresh queued.") }
        refreshResponse = ["queued": false]
        host.refreshConnection(refreshItem)
        try UICoverageTests.pump("Companion ignored a missing refresh acknowledgement") { messages.contains(where: { $0.contains("did not acknowledge") }) }
        for tick in timers {tick()}
        let menuItem=NSMenuItem();host.addDomain(menuItem)
        menuItem.representedObject=["id":"host"]
        host.addDomain(menuItem)
        host.enableFinder();host.openStorage();host.refreshConnections();host.quit()
        try expect(registrations==1 && enabled==1 && opened==1 && quit==1,"Companion actions bypassed their scoped boundaries")
        if let app=NSApp {host.application(app,open:[URL(string:"other://ignored")!,URL(string:"rdrive-native://message?text=URL%20note")!,URL(string:"rdrive-native://status?domain=host&id=file")!])}
        try UICoverageTests.pump("Companion URL message was lost") {messages.contains("URL note")}
        rows=[];host.rebuildMenu()
        try UICoverageTests.pump("Empty domain list missing") {host.menu.items.dropFirst().first?.submenu?.items.contains(where:{$0.title=="No connections configured"})==true && refreshMenu()?.items.first?.title == "No Smart connections configured"}
        requestError=companionTestError;host.rebuildMenu();host.showStatus(domain:"host",id:"file")
        try UICoverageTests.pump("Unavailable connection state missing") {host.menu.items.dropFirst().first?.submenu?.items.contains(where:{$0.title=="Connection service unavailable"})==true && refreshMenu()?.items.first?.title == "Connection service unavailable" && messages.contains(where:{$0.contains("SyntheticCompanion")})}
        requestError=nil;status=[:];host.showStatus(domain:"host",id:"file")
        try UICoverageTests.pump("Missing status did not preserve Unknown fallback") {messages.contains("Unknown")}
        host.applicationWillTerminate(terminated)
        var broken=environment;broken.paths={throw companionTestError}
        let unavailable=RDriveCompanionCoverage.CompanionDelegate(integration:broken)
        unavailable.applicationDidFinishLaunching(launched)
        try expect(unavailable.menu.items.first?.title=="Connection unavailable","Host initialization failure was not visible in its detached menu")
        unavailable.showStatus(domain:"missing",id:"missing")
        unavailable.applicationWillTerminate(terminated)
        requestError=companionTestError
        let spawning=RDriveCompanionCoverage.CompanionDelegate(integration:environment)
        spawning.applicationDidFinishLaunching(launched)
        try UICoverageTests.pump("Owned fake service was not launched") {process.launches==1}
        try expect(process.arguments.contains("--home") && process.arguments.contains(paths.group.path),"Owned service lost state-directory arguments")
        spawning.applicationWillTerminate(terminated)
        try expect(interrupted==[12345],"Companion interrupted a process it did not own")
        process.failure=companionTestError
        let failing=RDriveCompanionCoverage.CompanionDelegate(integration:environment)
        failing.applicationDidFinishLaunching(launched)
        try UICoverageTests.pump("Service launch failure was not reported") {messages.contains("RDrive could not start its connection service.")}
        failing.applicationWillTerminate(terminated)
        let stopped=RDriveCompanionCoverage.CompanionDelegate(integration:environment)
        stopped.applicationWillTerminate(terminated)
        stopped.startService(paths)
        let count=process.launches
        RunLoop.current.run(until:Date(timeIntervalSinceNow:0.05))
        try expect(process.launches==count,"A terminating Companion started another service")
        let actual=RDriveCompanionCoverage.NativeCompanionProcess()
        try actual.launch(executable:URL(fileURLWithPath:"/usr/bin/true"),arguments:[])
        try expect(actual.processIdentifier>0,"Synthetic actual subprocess had no identity")
        let deadline=Date(timeIntervalSinceNow:2)
        while actual.isRunning && Date()<deadline {Thread.sleep(forTimeInterval:0.01)}
        try expect(!actual.isRunning,"Synthetic actual subprocess did not finish")
        let missingProcess=RDriveCompanionCoverage.NativeCompanionProcess()
        try NativeTests.rejects("Missing service program unexpectedly launched") {try missingProcess.launch(executable:paths.group.appendingPathComponent("missing-program"),arguments:[])}
    }
    private static func ownedSignalAndTagBoundaries(_ paths:CompanionPaths) throws {
        // Signals target this disposable test process only; every original
        // disposition is restored after the GCD sources acknowledge cancellation.
        let oldInterrupt=Darwin.signal(SIGINT,SIG_IGN)
        let oldTerminate=Darwin.signal(SIGTERM,SIG_IGN)
        defer{Darwin.signal(SIGINT,oldInterrupt);Darwin.signal(SIGTERM,oldTerminate)}
        var delivered=0;var cancelled=0
        let live=RDriveCompanionCoverage.CompanionWatchIntegration.live
        let sources=live.signals{delivered += 1}
        sources.forEach{$0.setCancelHandler{cancelled += 1}}
        try expect(Darwin.kill(getpid(),SIGINT)==0,"Could not signal the isolated test process")
        try UICoverageTests.pump("Native watcher signal source did not receive its own test signal") {delivered==1}
        sources.forEach{$0.cancel()}
        try UICoverageTests.pump("Native signal sources did not finish cancellation") {cancelled==2}
        var ticks=0;let timer=live.timer{ticks += 1};timer.fire();timer.invalidate();live.wait()
        try expect(ticks==1,"Native timer did not invoke its own scoped callback")
        let file=paths.group.appendingPathComponent("coordinated-tags.txt")
        try Data("private tags fixture".utf8).write(to:file)
        let controls=RDriveCompanionCoverage.CompanionControlIntegration.live
        try controls.setTags(file,["Synthetic Tag"])
        try expect(try Data(contentsOf:file)==Data("private tags fixture".utf8),"Coordinated native tags changed file contents")
        try NativeTests.rejects("Tags on a missing synthetic file unexpectedly succeeded") {try controls.setTags(paths.group.appendingPathComponent("missing-tag-file"),[])}
    }

}
