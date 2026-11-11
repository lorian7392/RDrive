import Foundation
import AppKit
import FileProvider
import FinderSync
import ObjectiveC
@testable import RDriveCompanionCoverage

/// Replaces Objective-C dispatch only inside this disposable test process.
/// Every original IMP is restored, including when a Swift assertion throws.
private final class SDKMethodMasks {
    private var records: [(Method, IMP, IMP)] = []
    func replace(_ type: AnyClass, _ selector: String, classMethod: Bool = true, block: Any) throws {
        let name=NSSelectorFromString(selector)
        guard let method=classMethod ? class_getClassMethod(type,name) : class_getInstanceMethod(type,name) else {throw TestFailure(message:"SDK selector not found: "+selector)}
        let replacement=imp_implementationWithBlock(block)
        let original=method_setImplementation(method,replacement)
        records.append((method,original,replacement))
    }
    func restore() {
        for (method,original,replacement) in records.reversed() {method_setImplementation(method,original);imp_removeBlock(replacement)}
        records=[]
    }
    deinit {restore()}
}
private final class SDKEnumeratorProxy: NSObject, NSFileProviderEnumerator {
    var invalidated=false
    func invalidate(){invalidated=true}
    func enumerateItems(for observer:NSFileProviderEnumerationObserver,startingAt page:NSFileProviderPage){observer.finishEnumerating(upTo:nil)}
}
private final class SDKManagerProxy: NSObject {
    let url:URL
    let enumerator=SDKEnumeratorProxy()
    var downloads=0;var evictions=0;var signals=0
    var resolvedErrors:[NSError]=[]
    var resolutionFailure:NSError?
    init(_ url:URL){self.url=url}
    @objc(enumeratorForMaterializedItems) func materialized()->AnyObject {enumerator}
    @objc(getUserVisibleURLForItemIdentifier:completionHandler:)
    func visible(_ identifier:NSString,completion:@escaping(NSURL?,NSError?)->Void){completion(url as NSURL,nil)}
    @objc(evictItemWithIdentifier:completionHandler:)
    func evict(_ identifier:NSString,completion:@escaping(NSError?)->Void){evictions += 1;completion(nil)}
    @objc(signalEnumeratorForContainerItemIdentifier:completionHandler:)
    func signal(_ identifier:NSString,completion:@escaping(NSError?)->Void){signals += 1;completion(nil)}
    @objc(signalErrorResolved:completionHandler:)
    func resolved(_ error:NSError,completion:@escaping(NSError?)->Void){resolvedErrors.append(error);completion(resolutionFailure)}
    @objc(requestDownloadForItemWithIdentifier:requestedRange:completionHandler:)
    func download(_ identifier:NSString,range:NSRange,completion:@escaping(NSError?)->Void){downloads += 1;completion(nil)}
    @objc(temporaryDirectoryURLWithError:)
    func temporary(_ error:NSErrorPointer)->NSURL? {url as NSURL}
}
private final class SDKFinderProxy:NSObject {
    @objc var directoryURLs:NSSet=[]
    var registered=0;var badges=0
    let url:URL
    init(_ url:URL){self.url=url}
    @objc(setBadgeImage:label:forBadgeIdentifier:)
    func register(_ image:NSImage,label:NSString?,identifier:NSString){registered += 1}
    @objc(setBadgeIdentifier:forURL:)
    func badge(_ identifier:NSString,url:NSURL){badges += 1}
    @objc(selectedItemURLs) func selected()->[NSURL]? {[url as NSURL]}
    @objc(targetedURL) func target()->NSURL? {url as NSURL}
}
private final class SDKWorkspaceProxy:NSObject {
    var opened:[URL]=[]
    @objc(openURL:) func open(_ url:NSURL)->Bool {opened.append(url as URL);return true}
}
private final class SDKStatusButtonProxy:NSObject {@objc var image:NSImage?}
private final class SDKStatusItemProxy:NSObject {
    @objc var menu:NSMenu?
    @objc var button:SDKStatusButtonProxy?=SDKStatusButtonProxy()
}
private final class SDKStatusBarProxy:NSObject {
    let item=SDKStatusItemProxy()
    @objc(statusItemWithLength:) func make(_ length:Double)->AnyObject {item}
}
private typealias SDKDomainReply=@convention(block)(NSArray,NSError?)->Void
private typealias SDKErrorReply=@convention(block)(NSError?)->Void
private typealias SDKRemovedReply=@convention(block)(NSURL?,NSError?)->Void
private typealias SDKIdentityReply=@convention(block)(NSString?,NSString?,NSError?)->Void

@_silgen_name("rdrive_test_companion_entry")
private func invokeCompanionBootstrap(_ argc:Int32,_ argv:UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?)->Int32

enum NativeSDKCoverageTests {
    static func run(_ paths:NativePaths) throws {
        let masks=SDKMethodMasks();defer{masks.restore()}
        let manager=SDKManagerProxy(paths.group)
        let finder=SDKFinderProxy(paths.group)
        let workspace=SDKWorkspaceProxy()
        let statusBar=SDKStatusBarProxy()
        let domain=NSFileProviderDomain(identifier:NSFileProviderDomainIdentifier("sdk-fixture"),displayName:"SDK Fixture")
        var added=0;var removed=0;var settings=0;var modals=0;var appRuns=0;var quits=0
        let managerFactory:@convention(block)(AnyObject,NSFileProviderDomain)->AnyObject?={_,_ in manager}
        try masks.replace(NSFileProviderManager.self,"managerForDomain:",block:managerFactory)
        let domains:@convention(block)(AnyObject,@escaping SDKDomainReply)->Void={_,done in done([domain] as NSArray,nil)}
        try masks.replace(NSFileProviderManager.self,"getDomainsWithCompletionHandler:",block:domains)
        let add:@convention(block)(AnyObject,NSFileProviderDomain,@escaping SDKErrorReply)->Void={_,_,done in added += 1;done(nil)}
        try masks.replace(NSFileProviderManager.self,"addDomain:completionHandler:",block:add)
        let remove:@convention(block)(AnyObject,NSFileProviderDomain,UInt,@escaping SDKRemovedReply)->Void={_,_,mode,done in
            if mode==NSFileProviderManager.DomainRemovalMode.preserveDownloadedUserData.rawValue {removed += 1};done(paths.group as NSURL,nil)
        }
        try masks.replace(NSFileProviderManager.self,"removeDomain:mode:completionHandler:",block:remove)
        let identity:@convention(block)(AnyObject,NSURL,@escaping SDKIdentityReply)->Void={_,_,done in done("sdk-item","sdk-fixture",nil)}
        try masks.replace(NSFileProviderManager.self,"getIdentifierForUserVisibleFileAtURL:completionHandler:",block:identity)
        let finderFactory:@convention(block)(AnyObject)->AnyObject={_ in finder}
        try masks.replace(FIFinderSyncController.self,"defaultController",block:finderFactory)
        let enabled:@convention(block)(AnyObject)->Bool={_ in true}
        try masks.replace(FIFinderSyncController.self,"isExtensionEnabled",block:enabled)
        let enable:@convention(block)(AnyObject)->Void={_ in settings += 1}
        try masks.replace(FIFinderSyncController.self,"showExtensionManagementInterface",block:enable)
        let workspaceOpen:@convention(block)(AnyObject,NSURL)->Bool={_,url in workspace.open(url)}
        try masks.replace(NSWorkspace.self,"openURL:",classMethod:false,block:workspaceOpen)
        let statusFactory:@convention(block)(AnyObject)->AnyObject={_ in statusBar}
        try masks.replace(NSStatusBar.self,"systemStatusBar",block:statusFactory)
        let policy:@convention(block)(AnyObject,Int)->Bool={_,_ in true}
        try masks.replace(NSApplication.self,"setActivationPolicy:",classMethod:false,block:policy)
        let activate:@convention(block)(AnyObject,Bool)->Void={_,_ in}
        try masks.replace(NSApplication.self,"activateIgnoringOtherApps:",classMethod:false,block:activate)
        let modal:@convention(block)(AnyObject)->Int={_ in modals += 1;return NSApplication.ModalResponse.alertFirstButtonReturn.rawValue}
        try masks.replace(NSAlert.self,"runModal",classMethod:false,block:modal)
        let run:@convention(block)(AnyObject)->Void={_ in appRuns += 1}
        try masks.replace(NSApplication.self,"run",classMethod:false,block:run)
        let quit:@convention(block)(AnyObject,AnyObject?)->Void={_,_ in quits += 1}
        try masks.replace(NSApplication.self,"terminate:",classMethod:false,block:quit)

        let system=NativeSystemManager()
        var known:[NSFileProviderDomain]=[]
        system.domains{domains,_ in known=domains}
        try expect(known.first?.identifier==domain.identifier,"Default domain adapter did not use the mocked SDK completion")
        let native=system.manager(for:domain)!
        try expect(try native.temporaryDirectoryURL()==paths.group,"Default manager temporary directory forwarding changed")
        let observer=EnumerationProbe()
        native.enumeratorForMaterializedItems().enumerateItems(for:observer,startingAt:NSFileProviderPage(Data()))
        try wait(observer.done)
        var url:URL?
        native.getUserVisibleURL(for:providerID("sdk-item")){value,_ in url=value}
        native.signalEnumerator(for:.workingSet){_ in};native.requestDownloadForItem(withIdentifier:providerID("sdk-item")){_ in};native.evictItem(identifier:providerID("sdk-item")){_ in}
        try expect(url==paths.group && manager.signals==1 && manager.downloads==1 && manager.evictions==1,"Default manager adapter did not preserve SDK forwarding")
        let resolvedError=NSError(domain:NSFileProviderErrorDomain,code:NSFileProviderError.cannotSynchronize.rawValue)
        var resolutionReply:Error?
        native.signalErrorResolved(resolvedError){resolutionReply=$0}
        try expect(resolutionReply==nil && manager.resolvedErrors.last?.code==resolvedError.code,"Default manager did not forward the resolved error code")
        manager.resolutionFailure=NSError(domain:"SyntheticResolution",code:17)
        native.signalErrorResolved(resolvedError){resolutionReply=$0}
        try expect((resolutionReply as NSError?)===manager.resolutionFailure,"Default manager hid the native resolution callback failure")
        manager.resolutionFailure=nil
        let integration=NativeFinderIntegration.live
        integration.registerBadge("synthetic","icloud","Synthetic")
        integration.setRoots([paths.group]);integration.setBadge("cached",paths.group)
        try expect(integration.selection()==[paths.group] && integration.target()==paths.group,"Default Finder selection adapters changed")
        integration.identify(paths.group){item,_,_ in try? expect(item?.rawValue=="sdk-item","Default Finder identity adapter changed")}
        try expect(integration.manager(domain) != nil,"Default Finder manager factory changed")
        integration.open(URL(string:"rdrive-native://message?text=synthetic")!)
        try expect(finder.registered==1 && finder.badges==1 && finder.directoryURLs.count==1 && workspace.opened.count==1,"Default Finder adapters escaped the SDK mocks")
        var finderDefault:RDriveFinderSync?=RDriveFinderSync()
        try expect(finder.registered==6,"Default Finder initializer did not install its own five badge definitions")
        _=finderDefault?.menu(for:.contextualMenuForItems)
        finderDefault=nil
        let controls=RDriveCompanionCoverage.CompanionControlIntegration.live
        controls.addDomain(domain){_ in};controls.removeDomain(domain){_,_ in}
        controls.identify(paths.group){_,_,_ in};try expect(controls.finderEnabled(),"Default Finder enabled getter changed");controls.enableFinder()
        let host=RDriveCompanionCoverage.CompanionHostIntegration.live
        let menu=NSMenu(title:"Detached mocked status menu")
        _=host.configureStatus(menu)
        host.message("Synthetic message without a visible alert")
        host.enableFinder();host.openStorage();host.quit()
        try expect(statusBar.item.menu===menu && statusBar.item.button?.image != nil && modals==1 && settings==2 && added==1 && removed==1 && quits==1,"Default host adapters did not use their mocked SDK boundaries")
        if !paths.development {
            let original=NSApp.delegate
            let result=invokeCompanionBootstrap(0,nil)
            NSApp.delegate=original
            try expect(result==0 && appRuns==1,"Actual generated Companion bootstrap bypassed its mocked application boundary")
        }
        try expect(NSApp == nil || (!NSApp.isRunning && !NSApp.windows.contains(where:{$0.isVisible})),"Mocked SDK adapters displayed UI or ran an App loop")
        print("PASS default SDK adapter dispatch and actual Companion bootstrap with process-local Objective-C mocks, fully restored")
    }
}
