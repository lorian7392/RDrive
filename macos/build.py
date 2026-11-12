#!/usr/bin/env python3
"""Compile native bundles using the selected Apple SDK; never install/activate them."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import uuid

ROOT = Path(__file__).resolve().parent


def namespace_policy_supported(compiler):
    """Probe the selected SDK and compiler, rather than a Swift version number."""
    command = [str(argument) for argument in compiler] + ["-typecheck", "-"]
    baseline = subprocess.run(command, input="import FileProvider\n", text=True,
                              capture_output=True, check=False, timeout=120)
    if baseline.returncode:
        raise subprocess.CalledProcessError(baseline.returncode, command,
                                            output=baseline.stdout, stderr=baseline.stderr)
    source = """import FileProvider
@available(macOS 27.0, *)
func rdriveNamespacePolicyProbe() -> [NSFileProviderNamespacePolicy] {
    [.inherited, .materializeLazily, .materializeEagerly]
}
"""
    feature = subprocess.run(command, input=source, text=True,
                             capture_output=True, check=False, timeout=120)
    if feature.returncode not in (0, 1):
        raise subprocess.CalledProcessError(feature.returncode, command,
                                            output=feature.stdout, stderr=feature.stderr)
    return feature.returncode == 0


def run(*arguments):
    subprocess.run([str(x) for x in arguments], check=True)


def plist(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(plistlib.dumps(value, sort_keys=True))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--identity", default="-", help="codesign identity; '-' makes an ad-hoc review build")
    parser.add_argument("--team-id")
    parser.add_argument("--app-group")
    parser.add_argument("--development-ipc-root", type=Path, help="explicit private development-only IPC directory; adds only a narrow sandbox exception")
    parser.add_argument("--cli", type=Path, default=ROOT.parent / "target/debug/rdrive")
    parser.add_argument("--output", type=Path, default=ROOT / "build")
    options = parser.parse_args()
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    app_group = options.app_group or ((options.team_id + ".io.rdrive.shared") if options.team_id else "group.io.rdrive.shared")
    development_root = None
    if options.development_ipc_root:
        requested = options.development_ipc_root
        if not requested.is_absolute() or requested in (Path("/"), Path.home(), Path("/private/tmp")):
            raise SystemExit("Use a dedicated absolute development IPC directory, never a home or system directory.")
        development_root = requested.resolve()
        development_root.mkdir(parents=True, exist_ok=True, mode=0o700)
        metadata = development_root.stat()
        if metadata.st_uid != os.getuid() or metadata.st_mode & 0o077 or not development_root.is_dir():
            raise SystemExit("The explicit development IPC directory must be owned by this user with 0700 permissions.")
    final_app = output / "RDrive.app"
    build_root = output / (".building-" + uuid.uuid4().hex)
    app = build_root / "RDrive.app"
    sdk = subprocess.check_output(["xcrun", "--show-sdk-path"], text=True).strip()
    architecture = subprocess.check_output(["uname", "-m"], text=True).strip()
    shared = [ROOT / "Shared/Bridge.swift", ROOT / "Shared/TagData.swift", ROOT / "Shared/NativeBudget.swift"]
    flags = ["xcrun", "swiftc", "-swift-version", "5", "-sdk", sdk, "-target", architecture + "-apple-macosx13.0", "-O"]
    if development_root:
        flags += ["-D", "RDRIVE_DEVELOPMENT"]
    module_cache = output / "module-cache"
    flags += ["-module-cache-path", module_cache]
    namespace_policy = namespace_policy_supported(flags)
    if namespace_policy:
        flags += ["-D", "RDRIVE_HAS_NAMESPACE_POLICY"]
    base = {"CFBundleDevelopmentRegion": "en", "CFBundleVersion": "21", "CFBundleShortVersionString": "0.8.3",
            "LSMinimumSystemVersion": "13.0", "RDriveAppGroup": app_group}
    if development_root:
        base.pop("RDriveAppGroup")
        base["RDriveDevelopmentIPCRoot"] = str(development_root)
    application = dict(base, CFBundleIdentifier="io.rdrive.native", CFBundleName="RDrive", CFBundleExecutable="RDriveCompanion",
                       CFBundlePackageType="APPL", LSUIElement=True, NSPrincipalClass="NSApplication",
                       CFBundleURLTypes=[{"CFBundleURLName": "RDrive native controls", "CFBundleURLSchemes": ["rdrive-native"]}])
    plist(app / "Contents/Info.plist", application)
    binary = app / "Contents/MacOS/RDriveCompanion"
    binary.parent.mkdir(parents=True, exist_ok=True)
    run(*flags, "-module-name", "RDriveCompanion", *shared, ROOT / "Companion/DomainWatcher.swift", ROOT / "Companion/main.swift",
        "-framework", "Foundation", "-framework", "AppKit", "-framework", "FinderSync", "-framework", "FileProvider", "-o", binary)
    resources = app / "Contents/Resources"
    resources.mkdir(parents=True, exist_ok=True)
    if not options.cli.is_file():
        raise SystemExit("Build the Rust CLI first or provide --cli.")
    source_cli_sha256 = hashlib.sha256(options.cli.read_bytes()).hexdigest()
    cli_version = subprocess.check_output([str(options.cli.resolve()), "--version"], text=True).strip()
    shutil.copy2(options.cli, resources / "rdrive")
    entitlements = ({"com.apple.security.temporary-exception.files.absolute-path.read-write": [str(development_root) + "/"]}
                    if development_root else {"com.apple.security.application-groups": [app_group]})
    host_entitlements = output / "Companion.entitlements"
    plist(host_entitlements, entitlements)
    for name, identifier, principal, framework, sources in [
        ("RDriveFinder", "io.rdrive.native.finder", "RDriveFinderSync", "com.apple.FinderSync", [ROOT / "FinderSync/FinderSync.swift"]),
        ("RDriveProvider", "io.rdrive.native.provider", "RDriveProvider", "com.apple.fileprovider-nonui",
         [ROOT / "FileProvider/ProviderItem.swift", ROOT / "FileProvider/Enumerator.swift", ROOT / "FileProvider/Provider.swift"]),
        ("RDriveStatus", "io.rdrive.native.status", "RDriveStatusViewController", "com.apple.fileprovider-actionsui",
         [ROOT / "FileProviderUI/StatusViewController.swift"]),
    ]:
        bundle = app / ("Contents/PlugIns/" + name + ".appex")
        executable = bundle / ("Contents/MacOS/" + name)
        executable.parent.mkdir(parents=True, exist_ok=True)
        extension = {"NSExtensionPointIdentifier": framework, "NSExtensionPrincipalClass": principal}
        if framework == "com.apple.FinderSync":
            extension["NSExtensionAttributes"] = {}
        if framework == "com.apple.fileprovider-nonui":
            extension.update(NSExtensionFileProviderDocumentGroup=app_group, NSExtensionFileProviderSupportsEnumeration=True,
                NSExtensionFileProviderDownloadPipelineDepth=4, NSExtensionFileProviderUploadPipelineDepth=2,
                NSExtensionFileProviderActions=[
                    {"NSExtensionFileProviderActionIdentifier": "rdrive.refresh", "NSExtensionFileProviderActionName": "Refresh",
                     "NSExtensionFileProviderActionActivationRule": "fileproviderItems.@count == 1 AND "
                     "SUBQUERY(fileproviderItems, $item, $item.userInfo.rdriveSmart == YES AND $item.contentType.identifier == 'public.folder').@count == 1"},
                    {"NSExtensionFileProviderActionIdentifier": "rdrive.pin", "NSExtensionFileProviderActionName": "Keep Offline",
                     "NSExtensionFileProviderActionActivationRule": "TRUEPREDICATE"},
                    {"NSExtensionFileProviderActionIdentifier": "rdrive.unpin", "NSExtensionFileProviderActionName": "Allow Automatic Removal",
                     "NSExtensionFileProviderActionActivationRule": "TRUEPREDICATE"},
                ])
            titles = {"online": "Online only", "cached": "Available offline", "pinned": "Kept offline", "syncing": "Syncing",
                      "queued": "Waiting to sync", "blocked": "Excluded by sync rules", "conflict": "Sync conflict", "error": "Needs attention"}
            badges = {"cached": "checkmark", "pinned": "pinned", "syncing": "syncing", "queued": "in-review", "blocked": "in-review", "conflict": "warning", "error": "warning"}
            extension["NSFileProviderDecorations"] = [
                {"Identifier": "io.rdrive.native.provider.status." + state, "Category": "Sharing", "Label": title}
                for state, title in titles.items()
            ] + [
                {"Identifier": "io.rdrive.native.provider.badge." + state, "Category": "Badge", "Label": titles[state],
                 "BadgeImageType": "com.apple.icon-decoration.badge." + badge}
                for state, badge in badges.items()
            ]
            if development_root:
                extension.pop("NSExtensionFileProviderDocumentGroup")
        elif framework == "com.apple.fileprovider-actionsui":
            extension["NSExtensionFileProviderActions"] = [{
                "NSExtensionFileProviderActionIdentifier": "rdrive.status", "NSExtensionFileProviderActionName": "Show Sync Status",
                "NSExtensionFileProviderActionActivationRule": "TRUEPREDICATE"}, {
                "NSExtensionFileProviderActionIdentifier": "rdrive.repair", "NSExtensionFileProviderActionName": "Repair Sync Issue…",
                "NSExtensionFileProviderActionActivationRule": "fileproviderItems.@count == 1 AND "
                "SUBQUERY(fileproviderItems, $item, $item.userInfo.rdriveSmart == YES AND $item.userInfo.rdriveState IN {'conflict', 'blocked', 'queued'} "
                "AND $item.contentType.identifier == 'public.folder').@count == 1"}]
        plist(bundle / "Contents/Info.plist", dict(base, CFBundleIdentifier=identifier, CFBundleName=name,
            CFBundleExecutable=name, CFBundlePackageType="XPC!", NSExtension=extension))
        run(*flags, "-module-name", name, "-application-extension", "-parse-as-library", *shared, *sources,
            "-framework", "Foundation", "-framework", "AppKit", "-framework", "FinderSync", "-framework", "FileProvider",
            "-framework", "UniformTypeIdentifiers", "-framework", "FileProviderUI", "-Xlinker", "-e", "-Xlinker", "_NSExtensionMain", "-o", executable)
        extension_entitlements = output / (name + ".entitlements")
        plist(extension_entitlements, dict(entitlements, **{"com.apple.security.app-sandbox": True, "com.apple.security.network.client": True}))
        run("codesign", "--force", "--sign", options.identity, "--entitlements", extension_entitlements, bundle)
    run("codesign", "--force", "--sign", options.identity, resources / "rdrive")
    run("codesign", "--force", "--sign", options.identity, "--entitlements", host_entitlements, app)
    run("codesign", "--verify", "--deep", "--strict", "--verbose=2", app)
    embedded_cli_sha256 = hashlib.sha256((resources / "rdrive").read_bytes()).hexdigest()
    if final_app.exists():
        shutil.rmtree(final_app)
    app.rename(final_app)
    build_root.rmdir()
    report = {"app": str(final_app), "version": "0.8.3", "bundle_build": "21", "sdk": sdk, "architecture": architecture, "identity": options.identity, "team_id": options.team_id,
              "app_group": None if development_root else app_group, "development_ipc_root": str(development_root) if development_root else None,
              "distribution_ready": options.identity != "-" and development_root is None, "installed": False, "activated": False,
              "signing_status": "ad-hoc review build; no Developer Team identity" if options.identity == "-" else "signed with requested identity"}
    report.update(source_cli_sha256=source_cli_sha256, embedded_cli_sha256=embedded_cli_sha256,
                  cli_version=cli_version, sdk_namespace_policy=namespace_policy)
    (output / "build-report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
