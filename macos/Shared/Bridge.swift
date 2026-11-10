import Foundation
import FileProvider
import Darwin
import os

func nativeDevelopmentLog(_ message: String) {
    #if RDRIVE_DEVELOPMENT
        Logger(subsystem: "io.rdrive.native", category: "development").error("\(message, privacy: .public)")
    #endif
}

enum NativeFailure: Error {
    case invalid(String)
    case bridge(String)
}

struct NativePaths {
    let group: URL
    let development: Bool
    init(group: URL, development: Bool = false) { self.group = group; self.development = development }
    var socket: URL { group.appendingPathComponent("bridge.sock") }
    var transfers: URL { group.appendingPathComponent("Transfers", isDirectory: true) }
    var roots: URL { group.appendingPathComponent("finder-roots.json") }

    static func configured(bundle: Bundle = .main) throws -> NativePaths {
        if let path = bundle.object(forInfoDictionaryKey: "RDriveDevelopmentIPCRoot") as? String {
            let url = URL(fileURLWithPath: path, isDirectory: true).standardizedFileURL
            guard path.hasPrefix("/"), url.path == url.resolvingSymlinksInPath().path else {
                throw NativeFailure.invalid("The explicit development IPC directory must be a real absolute path.")
            }
            let attributes = try FileManager.default.attributesOfItem(atPath: path)
            guard attributes[.ownerAccountID] as? UInt32 == getuid(),
                  (attributes[.posixPermissions] as? UInt16 ?? 0) & 0o077 == 0,
                  attributes[.type] as? FileAttributeType == .typeDirectory else {
                throw NativeFailure.invalid("The explicit development IPC directory must be private and owned by this user.")
            }
            return NativePaths(group: url, development: true)
        }
        guard let identifier = bundle.object(forInfoDictionaryKey: "RDriveAppGroup") as? String,
              let group = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: identifier) else {
            throw NativeFailure.invalid("The signed app and its extensions require an authorized shared App Group.")
        }
        return NativePaths(group: group)
    }

    func prepare() throws {
        try FileManager.default.createDirectory(at: transfers, withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700])
    }
}

struct WireItem: Codable {
    let id: String
    let parent_id: String
    let name: String
    let kind: String
    let size: UInt64
    let modified: String?
    let content_version: String
    let metadata_version: String
    let pinned: Bool
    let capabilities: [String]
    let tag_data_hex: String?
    let extended_attributes: [String: String]?
    let sync_state: String?
    let sync_message: String?
    let smart: Bool?
    let has_unsynced_changes: Bool?
    let native_materialized: Bool?

    static func decode(_ object: Any) throws -> WireItem {
        let item = try JSONDecoder().decode(WireItem.self, from: JSONSerialization.data(withJSONObject: object))
        guard !item.id.isEmpty, !item.name.isEmpty, item.name != ".", item.name != "..",
              !item.name.contains("/"), !item.name.contains("\0"), ["file", "directory"].contains(item.kind),
              item.content_version.utf8.count <= 128, item.metadata_version.utf8.count <= 128 else {
            throw NativeFailure.invalid("Invalid item metadata received from the backend.")
        }
        var attributeBytes = 0
        for (name, hex) in item.extended_attributes ?? [:] where nativeSyncableAttribute(name) {
            guard hex.count % 2 == 0, hex.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else {
                throw NativeFailure.invalid("The backend returned invalid native attribute data.")
            }
            attributeBytes += hex.count / 2
            guard attributeBytes <= 32768 else { throw NativeFailure.invalid("Selected syncable attributes exceed File Provider's 32 KiB limit.") }
        }
        return item
    }
}

enum NativeSyncState: String, CaseIterable {
    case online, cached, pinned, syncing, queued, conflict, error
    var title: String {
        switch self {
        case .online: return "Online only"
        case .cached: return "Available offline"
        case .pinned: return "Kept offline"
        case .syncing: return "Syncing"
        case .queued: return "Waiting to sync"
        case .conflict: return "Sync conflict"
        case .error: return "Needs attention"
        }
    }
    var badgeType: String? {
        switch self {
        case .online: return nil // macOS owns the real dataless/download cloud indicator.
        case .cached: return "com.apple.icon-decoration.badge.checkmark"
        case .pinned: return "com.apple.icon-decoration.badge.pinned"
        case .syncing: return "com.apple.icon-decoration.badge.syncing"
        case .queued: return "com.apple.icon-decoration.badge.in-review"
        case .conflict, .error: return "com.apple.icon-decoration.badge.warning"
        }
    }
    static func item(_ item: WireItem) -> NativeSyncState {
        if let state = item.sync_state, let value = Self(rawValue: state) { return value }
        return item.pinned ? .pinned : .online
    }
}

func nativeSyncableAttribute(_ name: String) -> Bool {
    guard !name.isEmpty, !name.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
          !name.hasPrefix("com.apple.metadata:_kMDItemUserTags"), name.hasSuffix("#S") else { return false }
    // FinderInfo and tags have dedicated File Provider fields. Never forward
    // quarantine, resource forks, ACLs, security labels, or provider internals.
    return name.hasPrefix("user.") || name.hasPrefix("com.apple.metadata:")
}

func nativeAttributePayload(_ attributes: [String: Data]) throws -> [String: String] {
    var result: [String: String] = [:]
    var total = 0
    for (name, data) in attributes where nativeSyncableAttribute(name) {
        total += data.count
        guard total <= 32768 else { throw NativeFailure.invalid("Selected syncable attributes exceed File Provider's 32 KiB limit.") }
        result[name] = data.map { String(format: "%02x", $0) }.joined()
    }
    return result
}

func nativeHexData(_ value: String) -> Data? {
    guard value.count % 2 == 0 else { return nil }
    var result = Data()
    var index = value.startIndex
    while index < value.endIndex {
        let end = value.index(index, offsetBy: 2)
        guard let byte = UInt8(value[index..<end], radix: 16) else { return nil }
        result.append(byte); index = end
    }
    return result
}

struct WatchedRoot: Codable {
    let path: String
    let domain: String
}

func wireID(_ identifier: NSFileProviderItemIdentifier) -> String {
    if identifier == .rootContainer { return "root" }
    if identifier == .workingSet { return "working_set" }
    if identifier == .trashContainer { return "trash" }
    return identifier.rawValue
}

func providerID(_ identifier: String) -> NSFileProviderItemIdentifier {
    if identifier == "root" { return .rootContainer }
    if identifier == "working_set" { return .workingSet }
    if identifier == "trash" { return .trashContainer }
    return NSFileProviderItemIdentifier(identifier)
}

func providerError(_ error: Error) -> NSError {
    let ns = error as NSError
    if ns.domain == NSFileProviderErrorDomain || ns.domain == NSCocoaErrorDomain { return ns }
    return NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.serverUnreachable.rawValue,
        userInfo: [NSLocalizedDescriptionKey: String(describing: error)])
}

func remoteError(_ object: [String: Any]) -> NSError {
    let code = object["code"] as? String ?? "server_unreachable"
    let message = object["message"] as? String ?? "The backend request failed."
    if code == "cancelled" { return NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError) }
    if code == "unsupported" { return NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError,
        userInfo: [NSLocalizedDescriptionKey: message]) }
    let value: NSFileProviderError.Code
    switch code {
    case "no_such_item": value = .noSuchItem
    case "not_authenticated": value = .notAuthenticated
    case "filename_collision": value = .filenameCollision
    case "sync_anchor_expired": value = .syncAnchorExpired
    case "version_conflict": value = .cannotSynchronize
    case "insufficient_quota": value = .insufficientQuota
    case "directory_not_empty": value = .directoryNotEmpty
    case "deletion_rejected": value = .deletionRejected
    default: value = .serverUnreachable
    }
    return NSError(domain: NSFileProviderErrorDomain, code: value.rawValue,
        userInfo: [NSLocalizedDescriptionKey: message])
}

final class SocketOperation: @unchecked Sendable {
    private let lock = NSLock()
    private var descriptor: Int32 = -1
    private var cancelled = false
    private weak var progress: Progress?

    init(progress: Progress? = nil) { self.progress = progress }
    func report(completed: Int64, total: Int64) {
        guard total > 0, completed >= 0, completed <= total else { return }
        progress?.totalUnitCount = total
        progress?.completedUnitCount = completed
    }

    func attach(_ fd: Int32) throws {
        lock.lock(); defer { lock.unlock() }
        if cancelled { throw NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError) }
        descriptor = fd
    }
    func detach() { lock.lock(); descriptor = -1; lock.unlock() }
    func cancel() {
        lock.lock(); cancelled = true
        if descriptor >= 0 { _ = Darwin.shutdown(descriptor, SHUT_RDWR) }
        lock.unlock()
    }
    func check() throws {
        lock.lock(); let value = cancelled; lock.unlock()
        if value { throw NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError) }
    }
}

final class BackendBridge: @unchecked Sendable {
    let paths: NativePaths
    init(paths: NativePaths) { self.paths = paths }
    convenience init(bundle: Bundle = .main) throws { try self.init(paths: .configured(bundle: bundle)) }

    private func developmentEndpoint() throws -> (port: UInt16, token: String)? {
        guard paths.development else { return nil }
        let path = paths.group.appendingPathComponent("bridge-endpoint.json").path
        let descriptor = Darwin.open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard descriptor >= 0 else { throw NativeFailure.bridge("Start the RDrive companion to create its local development endpoint.") }
        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        var metadata = stat()
        guard fstat(descriptor, &metadata) == 0, metadata.st_uid == getuid(),
              metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_mode & 0o077 == 0 else {
            throw NativeFailure.invalid("The local development endpoint must be an owner-only regular file.")
        }
        let data = try handle.read(upToCount: 4097) ?? Data()
        guard data.count <= 4096, let value = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              value["transport"] as? String == "tcp", value["host"] as? String == "127.0.0.1",
              let number = value["port"] as? NSNumber, number.intValue > 0, number.intValue <= 65535,
              let token = value["token"] as? String, token.count == 64,
              token.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else {
            throw NativeFailure.invalid("The local development endpoint is invalid.")
        }
        return (UInt16(number.intValue), token)
    }

    func request(_ input: [String: Any], operation: SocketOperation? = nil) throws -> [String: Any] {
        try operation?.check()
        var request = input
        request["v"] = 1
        request["request_id"] = UUID().uuidString
        let tcp = try developmentEndpoint()
        if let tcp = tcp { request["auth_token"] = tcp.token }
        var payload = try JSONSerialization.data(withJSONObject: request)
        payload.append(10)
        guard payload.count <= 4 * 1024 * 1024 else { throw NativeFailure.invalid("Bridge request too large.") }
        let fd = Darwin.socket(tcp == nil ? AF_UNIX : AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw NativeFailure.bridge("Unable to create the local bridge socket.") }
        defer { operation?.detach(); Darwin.close(fd) }
        try operation?.attach(fd)
        var one: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout.size(ofValue: one)))
        var timeout = timeval(tv_sec: 120, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        let connected: Int32
        if let tcp = tcp {
            var address = sockaddr_in()
            address.sin_family = sa_family_t(AF_INET)
            address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
            address.sin_port = tcp.port.bigEndian
            guard inet_pton(AF_INET, "127.0.0.1", &address.sin_addr) == 1 else { throw NativeFailure.invalid("Invalid loopback address.") }
            connected = withUnsafePointer(to: &address) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
            }
        } else {
            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
            let socketPath = Array(paths.socket.path.utf8) + [0]
            guard socketPath.count <= MemoryLayout.size(ofValue: address.sun_path) else {
                throw NativeFailure.invalid("App Group path is too long for the current Unix socket client.")
            }
            withUnsafeMutableBytes(of: &address.sun_path) { bytes in bytes.copyBytes(from: socketPath) }
            connected = withUnsafePointer(to: &address) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
            }
        }
        guard connected == 0 else { throw NativeFailure.bridge("Start the RDrive companion to connect the backend (socket error \(errno)).") }
        try payload.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                try operation?.check()
                let count = Darwin.write(fd, raw.baseAddress!.advanced(by: offset), raw.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { try operation?.check(); throw NativeFailure.bridge("Backend request was interrupted.") }
                offset += count
            }
        }
        var reply = Data()
        var buffer = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            try operation?.check()
            if let newline = reply.firstIndex(of: 10) {
                let frame = reply.prefix(upTo: newline)
                guard let result = try JSONSerialization.jsonObject(with: frame) as? [String: Any] else { throw NativeFailure.invalid("Invalid backend response.") }
                reply.removeSubrange(...newline)
                if let event = result["progress"] as? [String: Any],
                   let completed = event["completed"] as? NSNumber, let total = event["total"] as? NSNumber {
                    operation?.report(completed: completed.int64Value, total: total.int64Value)
                    continue
                }
                guard result["ok"] as? Bool == true else { throw remoteError(result["error"] as? [String: Any] ?? [:]) }
                return result
            }
            let count = Darwin.read(fd, &buffer, buffer.count)
            if count < 0 && errno == EINTR { continue }
            guard count > 0 else { try operation?.check(); throw NativeFailure.bridge("Backend disconnected or timed out before replying.") }
            reply.append(contentsOf: buffer.prefix(count))
            guard reply.count <= 64 * 1024 * 1024 else { throw NativeFailure.invalid("Backend response too large.") }
        }
    }

    func item(_ request: [String: Any], operation: SocketOperation? = nil) throws -> WireItem {
        let result = try self.request(request, operation: operation)
        guard let item = result["item"] else { throw NativeFailure.invalid("Backend omitted the item.") }
        return try WireItem.decode(item)
    }

    func stagedCopy(_ source: URL) throws -> URL {
        try paths.prepare()
        let destination = paths.transfers.appendingPathComponent(UUID().uuidString)
        let scoped = source.startAccessingSecurityScopedResource()
        defer { if scoped { source.stopAccessingSecurityScopedResource() } }
        let values = try source.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
        guard values.isRegularFile == true, values.isSymbolicLink != true else {
            throw NativeFailure.invalid("Only regular file contents can be uploaded.")
        }
        try FileManager.default.copyItem(at: source, to: destination)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
        return destination
    }
}
