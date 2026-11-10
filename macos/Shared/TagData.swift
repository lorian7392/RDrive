import Foundation
import CoreFoundation

/// File Provider emits an abstract archive, while Finder's disk xattr is a
/// property-list array of name/color strings. Never write the archive as xattr.
func nativeFinderTagData(_ data: Data?) throws -> Data? {
    guard let data = data, !data.isEmpty else { return nil }
    guard data.count <= 1024 * 1024 else { throw NativeFailure.invalid("Native tag data exceeds the supported size.") }
    let plist = try PropertyListSerialization.propertyList(from: data, options: [], format: nil)
    if let tags = plist as? [String] {
        guard data.count <= 64 * 1024 else { throw NativeFailure.invalid("Finder tags exceed the supported 64 KiB size.") }
        try validateFinderTagStrings(tags)
        return data // Preserve an already canonical Finder xattr byte-for-byte.
    }
    guard let envelope = plist as? [String: Any], envelope["$archiver"] as? String == "NSKeyedArchiver" else {
        throw NativeFailure.invalid("Unsupported native tag format; local tag changes were not discarded.")
    }
    let classes: [AnyClass] = [NSDictionary.self, NSMutableDictionary.self, NSArray.self, NSMutableArray.self, NSString.self, NSNumber.self]
    guard let object = try NSKeyedUnarchiver.unarchivedObject(ofClasses: classes, from: data) as? [String: Any],
          Set(object.keys) == Set(["v", "t"]), let version = object["v"] as? NSNumber,
          CFGetTypeID(version) != CFBooleanGetTypeID(), version.doubleValue == 1,
          let pairs = object["t"] as? [Any], pairs.count <= 1024 else {
        throw NativeFailure.invalid("Unsupported native tag archive version; local tag changes were not discarded.")
    }
    var tags: [String] = []
    for entry in pairs {
        guard let pair = entry as? [Any], pair.count == 2, let name = pair[0] as? String,
              !name.isEmpty, name.rangeOfCharacter(from: .controlCharacters) == nil, name.utf8.count <= 1024,
              let color = pair[1] as? NSNumber, CFGetTypeID(color) != CFBooleanGetTypeID(),
              (0...7).contains(color.intValue), color.doubleValue == Double(color.intValue) else {
            throw NativeFailure.invalid("Invalid native tag name or color; local tag changes were not discarded.")
        }
        tags.append(name + "\n" + String(color.intValue))
    }
    guard !tags.isEmpty else { return nil }
    let normalized = try PropertyListSerialization.data(fromPropertyList: tags, format: .binary, options: 0)
    guard normalized.count <= 64 * 1024 else { throw NativeFailure.invalid("Finder tags exceed the supported 64 KiB size.") }
    return normalized
}

private func validateFinderTagStrings(_ tags: [String]) throws {
    guard tags.count <= 1024, tags.allSatisfy({ !$0.isEmpty && !$0.contains("\0") && $0.utf8.count <= 1030 }) else {
        throw NativeFailure.invalid("Invalid Finder tag array; local tag changes were not discarded.")
    }
}
