import Foundation
import CryptoKit
import Virtualization
import Darwin

// This helper owns one foreground VM. No network, shared directories, host
// control sockets or credentials are attached to the guest.
func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data(("ow-vz: " + message + "\n").utf8))
    exit(1)
}
func status(_ message: String) {
    FileHandle.standardError.write(Data(("ow-vz: " + message + "\n").utf8))
}

final class Lifecycle: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ virtualMachine: VZVirtualMachine) { status("guest stopped"); exit(0) }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        fail("guest stopped with error: \(error)")
    }
}

let args = Array(CommandLine.arguments.dropFirst())
if args == ["capabilities"] {
    let caps: [String: Any] = [
        "backend": "apple-virtualization", "architecture": "aarch64",
        "hardware_isolation": VZVirtualMachine.isSupported,
        "network": false, "shared_directories": false,
        "memory_snapshot": false, "hibernate": false, "live_fork": false,
        "pool_enrollment": false, "max_cpus": 1, "max_memory_mib": 512,
        "experimental_local_paired_capture": true, "stage": "local-runtime-spike"
    ]
    let data = try JSONSerialization.data(withJSONObject: caps, options: [.sortedKeys])
    FileHandle.standardOutput.write(data + Data("\n".utf8)); exit(0)
}
if args == ["macos-probe"] {
    // Metadata only: no IPSW download, installation or license acceptance.
    VZMacOSRestoreImage.fetchLatestSupported { result in
        switch result {
        case .failure(let error): fail("macOS restore metadata unavailable: \(error)")
        case .success(let image):
            let version = image.operatingSystemVersion
            var info: [String: Any] = [
                "version": "\(version.majorVersion).\(version.minorVersion).\(version.patchVersion)",
                "build": image.buildVersion, "supported": image.isSupported,
                "downloaded": false, "guest_boot_tested": false
            ]
            if let required = image.mostFeaturefulSupportedConfiguration {
                info["minimum_cpus"] = required.minimumSupportedCPUCount
                info["minimum_memory_mib"] = required.minimumSupportedMemorySize / (1024 * 1024)
            }
            do {
                let bytes = try JSONSerialization.data(withJSONObject: info, options: [.sortedKeys])
                FileHandle.standardOutput.write(bytes + Data("\n".utf8)); exit(0)
            } catch { fail("metadata encoding failed: \(error)") }
        }
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + 30) { fail("macOS metadata timed out") }
    RunLoop.main.run(); exit(1)
}
guard args.count == 4, ["boot", "restore"].contains(args[0]) else {
    fail("usage: ow-vz capabilities | boot KERNEL INITRAMFS DISK | restore KERNEL INITRAMFS PAIRED_DISK")
}
#if !arch(arm64)
fail("this prototype requires an Apple Silicon host and ARM64 boot inputs")
#endif
guard VZVirtualMachine.isSupported else { fail("Virtualization framework unsupported on this host") }
let paths = args.dropFirst().map { URL(fileURLWithPath: $0).standardizedFileURL }
for path in paths {
    var info = stat()
    guard lstat(path.path, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG,
          info.st_uid == getuid(), (info.st_mode & 0o022) == 0 else {
        fail("boot inputs must be owned regular files, without group/world write access")
    }
}
let lengths = try paths.map { try $0.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0 }
guard lengths[0] <= 128 * 1024 * 1024, lengths[1] <= 32 * 1024 * 1024,
      lengths[2] == 256 * 1024 * 1024 else { fail("fixture size/budget mismatch") }
let kernel = try Data(contentsOf: paths[0], options: [.mappedIfSafe])
guard kernel.count >= 64, Array(kernel[56..<60]) == [0x41, 0x52, 0x4d, 0x64] else {
    fail("kernel is not an uncompressed ARM64 Linux Image")
}
// Exclusive disk ownership spans VM lifetime; rejects a duplicate launch.
let root = paths[2].deletingLastPathComponent()
var rootInfo = stat()
guard lstat(root.path, &rootInfo) == 0, (rootInfo.st_mode & S_IFMT) == S_IFDIR,
      rootInfo.st_uid == getuid(), (rootInfo.st_mode & 0o077) == 0,
      paths.allSatisfy({ $0.deletingLastPathComponent() == root }) else {
    fail("all boot inputs must be in one private owned test root")
}
// Lock a separate sidecar: the framework exclusively locks the block file.
// One root-wide lock also enforces the initial one-guest budget.
let diskLock = open(root.appendingPathComponent(".runtime.lock").path,
                    O_RDWR | O_CREAT | O_NOFOLLOW, 0o600)
guard diskLock >= 0, flock(diskLock, LOCK_EX | LOCK_NB) == 0 else {
    fail("disk is already in use or cannot be locked")
}
let config = VZVirtualMachineConfiguration()
let platform = VZGenericPlatformConfiguration()
let identityURL = root.appendingPathComponent("machine-id")
if FileManager.default.fileExists(atPath: identityURL.path) {
    var identityInfo = stat()
    guard lstat(identityURL.path, &identityInfo) == 0,
          (identityInfo.st_mode & S_IFMT) == S_IFREG, identityInfo.st_uid == getuid(),
          (identityInfo.st_mode & 0o077) == 0,
          let identity = VZGenericMachineIdentifier(dataRepresentation: try Data(contentsOf: identityURL)) else {
        fail("invalid private machine identity")
    }
    platform.machineIdentifier = identity
} else {
    guard args[0] == "boot" else { fail("restore denied: missing original machine identity") }
    let identity = VZGenericMachineIdentifier()
    try identity.dataRepresentation.write(to: identityURL, options: .withoutOverwriting)
    guard chmod(identityURL.path, 0o600) == 0 else { fail("cannot protect machine identity") }
    platform.machineIdentifier = identity
}
config.platform = platform
let loader = VZLinuxBootLoader(kernelURL: paths[0])
loader.initialRamdiskURL = paths[1]
loader.commandLine = "console=hvc0 rdinit=/ow-init panic=-1"
config.bootLoader = loader
config.cpuCount = 1
config.memorySize = 512 * 1024 * 1024
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(fileHandleForReading: .standardInput,
                                                    fileHandleForWriting: .standardOutput)
config.serialPorts = [serial]
let attachment = try VZDiskImageStorageDeviceAttachment(url: paths[2], readOnly: false)
let disk = VZVirtioBlockDeviceConfiguration(attachment: attachment)
config.storageDevices = [disk]
do { try config.validate() } catch { fail("invalid configuration: \(error)") }
if #available(macOS 14.0, *) {
    do { try config.validateSaveRestoreSupport(); status("save/restore configuration supported (experimental local capture)") }
    catch { status("save/restore unsupported for this configuration: \(error)") }
}
let machine = VZVirtualMachine(configuration: config)
let lifecycle = Lifecycle()
machine.delegate = lifecycle
// Signals request bounded force-stop on the VM's main queue. Guest shutdown is
// preferred for disk consistency; the helper's process exit tears down its VM.
var signals: [DispatchSourceSignal] = []
for sig in [SIGTERM, SIGINT] {
    signal(sig, SIG_IGN)
    let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
    source.setEventHandler {
        if machine.canStop {
            machine.stop { result in
                if let error = result { fail("host stop failed: \(error)") }
                status("host stop complete"); exit(0)
            }
        } else { fail("interrupted before VM became stoppable") }
    }
    source.resume(); signals.append(source)
}
let checkpoint = root.appendingPathComponent("checkpoint", isDirectory: true)
func sha256(_ url: URL) throws -> String {
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var hash = SHA256()
    while let chunk = try handle.read(upToCount: 1024 * 1024), !chunk.isEmpty { hash.update(data: chunk) }
    return hash.finalize().map { String(format: "%02x", $0) }.joined()
}
func resumeGuest(_ message: String) {
    machine.resume { result in
        switch result {
        case .success: status(message)
        case .failure(let error): fail("resume failed: \(error)")
        }
    }
}
// Experimental host-only capture trigger. The caller must sync/quiesce guest
// workloads first. Paused capture is crash-consistent, not app-consistent.
signal(SIGUSR1, SIG_IGN)
let capture = DispatchSource.makeSignalSource(signal: SIGUSR1, queue: .main)
capture.setEventHandler {
    guard machine.canPause else { status("capture denied: VM not running"); return }
    guard !FileManager.default.fileExists(atPath: checkpoint.path) else {
        status("capture denied: immutable checkpoint already exists"); return
    }
    do { try FileManager.default.createDirectory(at: checkpoint, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700]) }
    catch { status("capture denied: \(error)"); return }
    machine.pause { result in
        switch result {
        case .failure(let error): fail("capture pause failed: \(error)")
        case .success:
            let state = checkpoint.appendingPathComponent("state.vz")
            machine.saveMachineStateTo(url: state) { error in
                if let error = error { fail("capture failed (incomplete directory retained): \(error)") }
                do {
                    let diskCopy = checkpoint.appendingPathComponent("disk.img")
                    guard clonefile(paths[2].path, diskCopy.path, 0) == 0 else {
                        fail("paused disk clone failed; capture retained incomplete")
                    }
                    let manifest: [String: Any] = [
                        "schema": 1, "backend": "apple-virtualization", "architecture": "aarch64",
                        "cpus": 1, "memory_mib": 512, "os": ProcessInfo.processInfo.operatingSystemVersionString,
                        "identity": try sha256(identityURL), "kernel": try sha256(paths[0]), "initramfs": try sha256(paths[1]),
                        "disk": try sha256(diskCopy), "state": try sha256(state)
                    ]
                    let bytes = try JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys])
                    try bytes.write(to: checkpoint.appendingPathComponent("manifest.json"), options: .atomic)
                    resumeGuest("OW_CAPTURE_READY")
                } catch { fail("capture manifest failed: \(error)") }
            }
        }
    }
}
capture.resume(); signals.append(capture)
if args[0] == "restore" {
    do {
        let manifestURL = checkpoint.appendingPathComponent("manifest.json")
        let manifest = try JSONSerialization.jsonObject(with: Data(contentsOf: manifestURL)) as? [String: Any]
        guard let manifest = manifest, manifest["schema"] as? Int == 1,
              manifest["backend"] as? String == "apple-virtualization",
              manifest["architecture"] as? String == "aarch64",
              manifest["cpus"] as? Int == 1, manifest["memory_mib"] as? Int == 512,
              manifest["os"] as? String == ProcessInfo.processInfo.operatingSystemVersionString,
              manifest["identity"] as? String == (try sha256(identityURL)),
              manifest["kernel"] as? String == (try sha256(paths[0])),
              manifest["initramfs"] as? String == (try sha256(paths[1])),
              manifest["disk"] as? String == (try sha256(paths[2])),
              manifest["state"] as? String == (try sha256(checkpoint.appendingPathComponent("state.vz"))) else {
            fail("restore denied: paired capture hash/configuration mismatch")
        }
        machine.restoreMachineStateFrom(url: checkpoint.appendingPathComponent("state.vz")) { error in
            if let error = error { fail("restore failed: \(error)") }
            resumeGuest("OW_RESTORE_READY")
        }
    } catch { fail("restore denied: \(error)") }
} else {
machine.start { result in
    switch result {
    case .success: status("VM started (guest readiness must be checked separately)")
    case .failure(let error): fail("VM start failed: \(error)")
    }
}
}
RunLoop.main.run()
