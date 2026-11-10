import Foundation
import FileProvider
import UniformTypeIdentifiers

final class ProviderItem: NSObject, NSFileProviderItem, NSFileProviderItemDecorating {
    let wire: WireItem
    init(_ wire: WireItem) { self.wire = wire }
    var itemIdentifier: NSFileProviderItemIdentifier { providerID(wire.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { providerID(wire.parent_id) }
    var filename: String { wire.name }
    var contentType: UTType {
        wire.kind == "directory" ? .folder : (UTType(filenameExtension: (wire.name as NSString).pathExtension) ?? .data)
    }
    var documentSize: NSNumber? { wire.kind == "directory" ? nil : NSNumber(value: wire.size) }
    var contentModificationDate: Date? {
        guard let value = wire.modified else { return nil }
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: value) { return date }
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: value)
    }
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data(wire.content_version.utf8), metadataVersion: Data(wire.metadata_version.utf8))
    }
    var capabilities: NSFileProviderItemCapabilities {
        var result: NSFileProviderItemCapabilities = []
        for capability in wire.capabilities {
            switch capability {
            case "read": result.insert(.allowsReading)
            case "write", "add": result.insert(.allowsWriting)
            case "rename": result.insert(.allowsRenaming)
            case "reparent": result.insert(.allowsReparenting)
            case "delete": result.insert(.allowsDeleting)
            default: break
            }
        }
        return result
    }
    var contentPolicy: NSFileProviderContentPolicy {
        if wire.pinned { return .downloadEagerlyAndKeepDownloaded }
        return wire.id == "root" ? .downloadLazily : .inherited
    }
    @available(macOS 27.0, *)
    var namespacePolicy: NSFileProviderNamespacePolicy {
        if wire.kind == "directory" && wire.pinned { return .materializeEagerly }
        return wire.id == "root" ? .materializeLazily : .inherited
    }
    var isUploaded: Bool { !(wire.has_unsynced_changes ?? [.syncing, .queued, .conflict].contains(NativeSyncState.item(wire))) }
    var isUploading: Bool { NativeSyncState.item(wire) == .syncing }
    var uploadingError: Error? {
        switch NativeSyncState.item(wire) {
        case .conflict: return remoteError(["code": "version_conflict", "message": wire.sync_message ?? "The local change conflicts with the remote version."])
        case .error: return remoteError(["code": "server_unreachable", "message": wire.sync_message ?? "The storage connection needs attention."])
        default: return nil
        }
    }
    var decorations: [NSFileProviderItemDecorationIdentifier]? {
        guard wire.id != "root" else { return nil }
        let state = NativeSyncState.item(wire)
        var values = [NSFileProviderItemDecorationIdentifier("io.rdrive.native.provider.status." + state.rawValue)]
        if state.badgeType != nil { values.append(NSFileProviderItemDecorationIdentifier("io.rdrive.native.provider.badge." + state.rawValue)) }
        return values
    }
    var tagData: Data? {
        wire.tag_data_hex.flatMap(nativeHexData)
    }
    var extendedAttributes: [String: Data] {
        var result: [String: Data] = [:]
        for (name, value) in wire.extended_attributes ?? [:] where nativeSyncableAttribute(name) {
            if let data = nativeHexData(value) { result[name] = data }
        }
        return result
    }
    var userInfo: [AnyHashable: Any]? {
        ["rdrivePinned": wire.pinned, "rdriveState": NativeSyncState.item(wire).rawValue, "rdriveSmart": wire.smart ?? false]
    }
}
