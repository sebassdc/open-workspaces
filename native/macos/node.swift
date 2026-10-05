import Darwin
// Native persistent Linux node helper: fixed private sockets, no guest networking.
import Foundation
import Virtualization

private func ownedRegular(_ path: String) -> Bool {
  var info = stat()
  return lstat(path, &info) == 0 && (info.st_mode & S_IFMT) == S_IFREG
    && info.st_uid == getuid() && (info.st_mode & 0o022) == 0
}
private func socketListener(_ path: String) throws -> Int32 {
  guard path.utf8.count < 104 else { throw NSError(domain: "ow-vz", code: 1) }
  let fd = socket(AF_UNIX, SOCK_STREAM, 0)
  guard fd >= 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
  var address = sockaddr_un()
  address.sun_family = sa_family_t(AF_UNIX)
  address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
  withUnsafeMutableBytes(of: &address.sun_path) { storage in
    storage.copyBytes(from: Array(path.utf8) + [0])
  }
  let bound = withUnsafePointer(to: &address) {
    $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
      Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
    }
  }
  guard bound == 0, chmod(path, 0o600) == 0, listen(fd, 16) == 0 else {
    close(fd)
    throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno))
  }
  return fd
}
private func writeBounded(_ fd: Int32, _ bytes: UnsafeRawPointer, _ count: Int) -> Bool {
  var sent = 0
  let deadline = Date().addingTimeInterval(5)
  while sent < count && Date() < deadline {
    let n = write(fd, bytes.advanced(by: sent), count - sent)
    if n > 0 {
      sent += n
      continue
    }
    if n < 0 && (errno == EAGAIN || errno == EINTR) {
      var event = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
      _ = poll(&event, 1, 100)
      continue
    }
    return false
  }
  return sent == count
}
private func bridge(_ client: Int32, _ connection: VZVirtioSocketConnection, _ seconds: Double) {
  let guest = connection.fileDescriptor
  defer {
    close(client)
    connection.close()
  }
  for fd in [client, guest] {
    _ = fcntl(fd, F_SETFL, O_NONBLOCK)
    var yes: Int32 = 1
    _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &yes, socklen_t(MemoryLayout<Int32>.size))
  }
  let expires = Date().addingTimeInterval(seconds)
  var idle = Date()
  var buffer = [UInt8](repeating: 0, count: 16384)
  while Date() < expires && Date().timeIntervalSince(idle) < 90 {
    var events = [
      pollfd(fd: client, events: Int16(POLLIN), revents: 0),
      pollfd(fd: guest, events: Int16(POLLIN), revents: 0),
    ]
    let ready = poll(&events, 2, 100)
    if ready < 0 {
      if errno == EINTR { continue }
      return
    }
    for i in 0..<2 where events[i].revents != 0 {
      let source = i == 0 ? client : guest
      let target = i == 0 ? guest : client
      let n = read(source, &buffer, buffer.count)
      if n == 0 { return }
      if n < 0 {
        if errno == EAGAIN || errno == EINTR { continue }
        return
      }
      idle = Date()
      let ok = buffer.withUnsafeBytes { writeBounded(target, $0.baseAddress!, n) }
      if !ok { return }
    }
  }
}
final class NativeNode {
  let vm: VZVirtualMachine
  let delegate = Lifecycle()
  var signals: [DispatchSourceSignal] = []
  var parentWatch: DispatchSourceRead?
  var stopping = false
  let slots = DispatchSemaphore(value: 16)
  init(_ configURL: URL) throws {
    guard ownedRegular(configURL.path) else { fail("private node config required") }
    let root = configURL.deletingLastPathComponent()
    var info = stat()
    guard lstat(root.path, &info) == 0, info.st_uid == getuid(), (info.st_mode & 0o077) == 0 else {
      fail("private node root required")
    }
    let value =
      try JSONSerialization.jsonObject(with: Data(contentsOf: configURL)) as! [String: Any]
    let kernel = value["kernel"] as! String
    let initrd = value["initrd"] as! String
    let diskPath = root.appendingPathComponent("root.ext4").path
    guard [kernel, initrd, diskPath].allSatisfy(ownedRegular) else {
      fail("owned boot inputs required")
    }
    let data = try Data(contentsOf: URL(fileURLWithPath: kernel), options: .mappedIfSafe)
    guard data.count >= 64, Array(data[56..<60]) == [0x41, 0x52, 0x4d, 0x64] else {
      fail("ARM64 Image required")
    }
    let memory = value["memory_mib"] as! UInt64
    let cpus = value["vcpu_count"] as! Int
    guard [512, 1024, 2048].contains(memory), (1...2).contains(cpus) else {
      fail("native shape unsupported")
    }
    let lock = open(
      root.appendingPathComponent("vm.lock").path, O_RDWR | O_CREAT | O_NOFOLLOW, 0o600)
    guard lock >= 0, flock(lock, LOCK_EX | LOCK_NB) == 0 else { fail("native VM already active") }
    let config = VZVirtualMachineConfiguration()
    let loader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: kernel))
    loader.initialRamdiskURL = URL(fileURLWithPath: initrd)
    loader.commandLine =
      "console=hvc0 root=/dev/vda rw init=/ow-node-init panic=0 ow_epoch=\(Int64(Date().timeIntervalSince1970))"
    config.bootLoader = loader
    config.cpuCount = cpus
    config.memorySize = memory * 1024 * 1024
    let platform = VZGenericPlatformConfiguration()
    let idURL = root.appendingPathComponent("machine-id")
    if FileManager.default.fileExists(atPath: idURL.path) {
      guard ownedRegular(idURL.path),
        let id = VZGenericMachineIdentifier(dataRepresentation: try Data(contentsOf: idURL))
      else { fail("invalid machine identity") }
      platform.machineIdentifier = id
    } else {
      let id = VZGenericMachineIdentifier()
      try id.dataRepresentation.write(to: idURL, options: .withoutOverwriting)
      chmod(idURL.path, 0o600)
      platform.machineIdentifier = id
    }
    config.platform = platform
    config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
    config.socketDevices = [VZVirtioSocketDeviceConfiguration()]
    let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
    serial.attachment = VZFileHandleSerialPortAttachment(
      fileHandleForReading: nil, fileHandleForWriting: .standardError)
    config.serialPorts = [serial]
    config.storageDevices = [
      VZVirtioBlockDeviceConfiguration(
        attachment: try VZDiskImageStorageDeviceAttachment(
          url: URL(fileURLWithPath: diskPath), readOnly: false))
    ]
    try config.validate()
    vm = VZVirtualMachine(configuration: config)
    vm.delegate = delegate
    signal(SIGPIPE, SIG_IGN)
    for sig in [SIGTERM, SIGINT] {
      signal(sig, SIG_IGN)
      let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
      source.setEventHandler { [self] in shutdown() }
      source.resume()
      signals.append(source)
    }
    // The Rust worker owns this pipe. Its exit (including SIGKILL) must
    // stop the helper rather than leave an unaccounted native guest.
    let watch = DispatchSource.makeReadSource(fileDescriptor: STDIN_FILENO, queue: .main)
    watch.setEventHandler { [self] in
      var byte: UInt8 = 0
      if read(STDIN_FILENO, &byte, 1) == 0 { shutdown() }

    }
    watch.resume()
    parentWatch = watch
    for (name, port, duration) in [
      ("rpc.sock", UInt32(7000), Double(40)), ("pty.sock", UInt32(7001), Double(3600)),
    ] {
      let fd = try socketListener(root.appendingPathComponent(name).path)
      DispatchQueue.global().async { [self] in
        while true {
          let client = accept(fd, nil, nil)
          if client < 0 { continue }
          var uid: uid_t = 0
          var gid: gid_t = 0
          guard getpeereid(client, &uid, &gid) == 0, uid == getuid(),
            slots.wait(timeout: .now()) == .success
          else {
            close(client)
            continue
          }
          DispatchQueue.main.async { [self] in
            guard vm.state == .running, let socket = vm.socketDevices.first as? VZVirtioSocketDevice
            else {
              close(client)
              slots.signal()
              return
            }
            socket.connect(toPort: port) { [self] result in
              switch result {
              case .failure:
                close(client)
                slots.signal()
              case .success(let connection):
                DispatchQueue.global().async { [self] in
                  bridge(client, connection, duration)
                  slots.signal()
                }
              }
            }
          }
        }
      }
    }
  }
  func shutdown() {
    guard !stopping else { return }
    stopping = true
    parentWatch?.cancel()
    stopWhenReady()
  }
  func stopWhenReady() {
    if vm.canStop {
      vm.stop { error in
        if let error = error { fail("native stop failed: \(error)") }
        status("native stop complete")
        exit(0)
      }
    } else if vm.state == .starting || vm.state == .stopping {
      DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [self] in stopWhenReady() }
    } else if vm.state == .stopped {
      exit(0)
    } else {
      fail("native stop unavailable after parent exit")
    }
  }
  func start() {
    vm.start { result in
      switch result {
      case .success: status("native VM started; RPC readiness pending")
      case .failure(let e): fail("native start failed: \(e)")
      }
    }
  }
}
func runNativeNode(_ args: [String]) -> Never {
  guard args.count == 2 else { fail("usage: ow-vz node PRIVATE_CONFIG") }
  do {
    let node = try NativeNode(URL(fileURLWithPath: args[1]))
    node.start()
    withExtendedLifetime(node) { RunLoop.main.run() }
    exit(1)
  } catch { fail("native node configuration failed: \(error)") }
}
