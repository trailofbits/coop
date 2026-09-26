import Containerization
import ContainerizationExtras
import ContainerizationOCI
import Foundation

/// The process that owns one running sandbox VM. Virtualization.framework
/// runs the VM in-process, so the owner's lifetime bounds the VM's.
public enum Owner {
    /// systemd's "halt" request: SIGRTMIN+3 on Linux (SIGRTMIN = 34).
    static let systemdHalt = Signal(rawValue: 37)
    static let haltGraceSeconds: UInt64 = 60

    public static func run(root: SandboxRoot, id: SandboxID) async throws {
        try root.requireInitialized()
        let paths = root.sandbox(id)
        signal(SIGPIPE, SIG_IGN)
        // Held until the process exits; the kernel then releases it.
        _ = try await claim(root: root, id: id)
        // A stale live.json means the previous owner died without cleanup.
        try? FileManager.default.removeItem(at: paths.live)

        var record = try paths.loadRecord()
        let vmm = VZVirtualMachineManager(
            kernel: Kernel(path: root.kernel, platform: .linuxArm),
            initialFilesystem: .block(format: "ext4", source: root.initfs.path, destination: "/", options: ["ro"])
        )
        // One vmnet network per sandbox: the sandbox is the only peer on it.
        var network = try makeNetwork(root: root, paths: paths, record: &record)
        guard let interface = try network.createInterface(id.rawValue) else {
            throw SandboxError("vmnet returned no interface")
        }

        let rootfs = Containerization.Mount.block(format: "ext4", source: paths.rootfs.path, destination: "/", options: [])
        let console = try ConsoleLog(url: paths.bootLog)
        let config = machineConfiguration(record: record, interface: interface, bootLog: .fileHandle(console.writer))
        let container = try LinuxContainer(id.rawValue, rootfs: rootfs, vmm: vmm, configuration: config)
        try await container.create()
        try await container.start()

        let live = LiveState(
            pid: getpid(), startedAt: Date(),
            ipv4: interface.ipv4Address.address.description,
            ipv6: interface.ipv6Address?.address.description)
        try JSONEncoder.pretty.encode(live).write(to: paths.live, options: .atomic)
        log("started pid=\(live.pid) ipv4=\(live.ipv4 ?? "-") ipv6=\(live.ipv6 ?? "-")")

        let lifecycle = Lifecycle(container: container)
        let effective = effectiveConfig(record: record, config: config, rootfs: rootfs, interface: interface)
        try ControlSocket.serve(at: paths.control) { request in
            switch request.op {
            case .ping:
                return ControlResponse(ok: true)
            case .inspect:
                return ControlResponse(ok: true, inspect: effective)
            case .exec:
                guard let argv = request.argv, !argv.isEmpty else { return ControlResponse(ok: false, error: "empty argv") }
                do {
                    return try await exec(container, argv, stdin: request.stdin, timeout: request.timeoutSeconds)
                } catch {
                    return ControlResponse(ok: false, error: "\(error)")
                }
            case .stop:
                await lifecycle.requestHalt()
                await lifecycle.waitStopped()
                return ControlResponse(ok: true)
            }
        }

        signal(SIGTERM, SIG_IGN)
        signal(SIGINT, SIG_IGN)
        let signalSources = [SIGTERM, SIGINT].map { sig in
            let source = DispatchSource.makeSignalSource(signal: sig)
            source.setEventHandler {
                log("signal \(sig): halting")
                Task { await lifecycle.requestHalt() }
            }
            source.resume()
            return source
        }

        // The machine ends when its init exits: a halt request or a guest poweroff.
        let status = try? await container.wait()
        log("init exited status=\(status?.exitCode ?? -1)")
        do {
            try await container.stop()
        } catch {
            log("stop error: \(error)")
        }
        try? network.releaseInterface(id.rawValue)
        try? FileManager.default.removeItem(at: paths.live)
        await lifecycle.markStopped()
        unlink(paths.control.path)
        _ = signalSources
        // Let in-flight `stop` responses flush.
        try? await Task.sleep(for: .milliseconds(200))
    }

    /// Take `id`'s owner lock for the owner's whole life, under the mutation
    /// guard, however the owner was launched: an owner never starts
    /// mid-update, and later mutations see it and refuse. An interrupted disk
    /// update is settled first. The guard is released on return.
    static func claim(root: SandboxRoot, id: SandboxID) async throws -> Int32 {
        let paths = root.sandbox(id)
        let guarded = try await FileLock.acquire(paths.mutationLock, .exclusive, polling: .milliseconds(50))
        defer { withExtendedLifetime(guarded) {} }
        let fd = open(paths.lock.path, O_CREAT | O_RDWR | O_CLOEXEC, 0o600)
        guard fd >= 0 else { throw SandboxError("open \(paths.lock.path): errno \(errno)") }
        do {
            guard try await acquireOwnership(fd) else { throw SandboxError("\(id) already has an owner") }
            try DiskUpdate.settle(paths)
            return fd
        } catch {
            close(fd)
            throw error
        }
    }

    /// `status` probes the owner lock with a momentary non-blocking flock, so
    /// retry briefly rather than mistake a probe for a second owner.
    static func acquireOwnership(_ fd: Int32) async throws -> Bool {
        for _ in 0..<20 {
            if flock(fd, LOCK_EX | LOCK_NB) == 0 { return true }
            try await Task.sleep(for: .milliseconds(100))
        }
        return false
    }

    /// vmnet keeps a subnet reserved for hours after its owning process dies
    /// uncleanly, so a restarted sandbox may not get its old subnet back, and
    /// several neighbours may be taken the same way. Quarantine each refused
    /// subnet and move on (the address changes; the identity does not).
    static let maxNetworkAttempts = 32

    static func makeNetwork(root: SandboxRoot, paths: SandboxPaths, record: inout SandboxRecord) throws -> VmnetNetwork {
        var attempts = 0
        while true {
            do {
                return try VmnetNetwork(subnet: try CIDRv4(record.subnet))
            } catch {
                attempts += 1
                guard attempts < maxNetworkAttempts else { throw error }
                let failed = record.subnetIndex
                var updated = record
                try SubnetAllocator(root: root).quarantineAndReallocate(failed, for: record.id) { next in
                    updated.subnetIndex = next
                    try paths.save(updated)
                }
                record = updated
                log("subnet \(SandboxRecord.subnet(failed)) unavailable (\(error)); moved to \(record.subnet)")
            }
        }
    }

    static func machineConfiguration(record: SandboxRecord, interface: any Interface, bootLog: BootLog) -> LinuxContainer.Configuration {
        var config = LinuxContainer.Configuration()
        var process = LinuxProcessConfiguration()
        process.arguments = ["/sbin/init"]
        process.environmentVariables = record.environment
        process.workingDirectory = "/"
        process.user = .init()
        process.capabilities = .allCapabilities
        process.terminal = false
        config.process = process
        config.cpus = record.cpus
        config.memoryInBytes = record.memoryBytes
        config.hostname = record.id.rawValue
        config.interfaces = [interface]
        if let gateway = interface.ipv4Gateway {
            config.dns = DNS(nameservers: [gateway.description])
        }
        var hosts = Hosts.default
        hosts.entries.append(.init(ipAddress: interface.ipv4Address.address.description, hostnames: [record.id.rawValue]))
        config.hosts = hosts
        // Kernel pseudo-filesystems only; no host shares, no socket relays.
        config.mounts = LinuxContainer.defaultMounts()
        config.sockets = []
        // systemd and dockerd need to write cgroup/sysctl state.
        config.maskedPaths = []
        config.readonlyPaths = []
        config.virtualization = false
        config.bootLog = bootLog
        return config
    }

    static func effectiveConfig(
        record: SandboxRecord, config: LinuxContainer.Configuration, rootfs: Containerization.Mount, interface: any Interface
    ) -> EffectiveConfig {
        func view(_ m: Containerization.Mount) -> EffectiveConfig.MountView {
            .init(type: m.type, source: m.source, destination: m.destination, options: m.options)
        }
        return EffectiveConfig(
            id: record.id.rawValue,
            imageReference: record.imageReference,
            imageDigest: record.imageDigest,
            cpus: config.cpus,
            memoryBytes: config.memoryInBytes,
            rootfs: view(rootfs),
            mounts: config.mounts.map(view),
            interfaces: config.interfaces.map {
                .init(
                    ipv4: $0.ipv4Address.description, ipv4Gateway: $0.ipv4Gateway?.description,
                    ipv6: $0.ipv6Address?.description, network: "vmnet-shared:\(record.subnet)")
            },
            socketRelays: config.sockets.count,
            publishedPorts: 0,
            sshAgentForwarding: false,
            maskedPaths: config.maskedPaths,
            readonlyPaths: config.readonlyPaths,
            initArgv: config.process.arguments,
            virtualization: config.virtualization
        )
    }

    static func exec(_ container: LinuxContainer, _ argv: [String], stdin: Data?, timeout: Int64?) async throws -> ControlResponse {
        let out = BufferWriter()
        let err = BufferWriter()
        let input = stdin.map(DataReader.init)
        let process = try await container.exec("x" + UUID().uuidString.prefix(12).lowercased()) { c in
            c.arguments = argv
            c.environmentVariables = ["PATH=\(LinuxProcessConfiguration.defaultPath)", "HOME=/root"]
            c.workingDirectory = "/"
            c.user = .init()
            c.capabilities = .allCapabilities
            c.stdout = out
            c.stderr = err
            c.stdin = input
        }
        try await process.start()
        let status: ExitStatus
        do {
            status = try await process.wait(timeoutInSeconds: timeout)
        } catch {
            // A timed-out command must not keep running in the guest.
            try? await process.kill(.kill)
            try? await process.delete()
            throw error
        }
        try? await process.delete()
        return ControlResponse(ok: true, exitCode: status.exitCode, stdout: out.data, stderr: err.data)
    }

    static func log(_ message: String) {
        let line = "\(ISO8601DateFormatter().string(from: Date())) \(message)\n"
        FileHandle.standardError.write(Data(line.utf8))
    }
}

actor Lifecycle {
    private let container: LinuxContainer
    private var halting = false
    private var stopped = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(container: LinuxContainer) { self.container = container }

    /// Ask systemd to shut down cleanly; force the VM down if it has not
    /// halted within the grace period.
    func requestHalt() {
        guard !halting else { return }
        halting = true
        Owner.log("halt requested")
        let container = self.container
        Task {
            try? await container.kill(Owner.systemdHalt)
            try? await Task.sleep(for: .seconds(Owner.haltGraceSeconds))
            if !self.isStopped {
                try? await container.kill(.kill)
            }
        }
    }

    var isStopped: Bool { stopped }

    func markStopped() {
        stopped = true
        waiters.forEach { $0.resume() }
        waiters.removeAll()
    }

    func waitStopped() async {
        if stopped { return }
        await withCheckedContinuation { waiters.append($0) }
    }
}

/// Collects guest output up to `limit` bytes; the rest is dropped, so a
/// guest cannot exhaust the owner's memory.
final class BufferWriter: Writer, @unchecked Sendable {
    static let limit = 8 * 1024 * 1024
    private let lock = NSLock()
    private var buffer = Data()
    func write(_ data: Data) throws {
        lock.withLock { buffer.append(data.prefix(max(0, Self.limit - buffer.count))) }
    }
    func close() throws {}
    var data: Data { lock.withLock { buffer } }
}

struct DataReader: ReaderStream {
    let data: Data
    func stream() -> AsyncStream<Data> {
        AsyncStream { continuation in
            if !data.isEmpty { continuation.yield(data) }
            continuation.finish()
        }
    }
}

/// The host copy of the serial console. Guest root writes the console
/// freely, so the file is capped: past `limit` bytes it restarts with a
/// marker line. It is appended to across boots up to the same cap.
final class ConsoleLog: Sendable {
    static let limit = 8 * 1024 * 1024
    private let pipe = Pipe()
    var writer: FileHandle { pipe.fileHandleForWriting }

    init(url: URL) throws {
        if !FileManager.default.fileExists(atPath: url.path) {
            FileManager.default.createFile(atPath: url.path, contents: nil, attributes: [.posixPermissions: 0o600])
        }
        let out = try FileHandle(forWritingTo: url)
        var start = try out.seekToEnd()
        if start > Self.limit {
            try out.truncate(atOffset: 0)
            start = 0
        }
        let reader = pipe.fileHandleForReading
        let written = Int(start)
        Thread.detachNewThread {
            ConsoleLog.copy(from: reader, to: out, written: written)
        }
    }

    /// Drains `reader` into `out` until EOF. Uses throwing I/O only: the
    /// legacy FileHandle calls raise uncatchable exceptions (a full host disk
    /// would kill the owner and its VM). After a write error it keeps
    /// draining and discards, so the guest console never blocks.
    static func copy(from reader: FileHandle, to out: FileHandle, written: Int, limit: Int = limit) {
        var written = written
        var writable = true
        var buffer = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            let n = read(reader.fileDescriptor, &buffer, buffer.count)
            if n < 0, errno == EINTR { continue }
            if n <= 0 { return }
            guard writable else { continue }
            var data = Data(buffer[0..<n])
            do {
                if written + data.count > limit {
                    try out.truncate(atOffset: 0)
                    let marker = Data("[coop-sandbox: console log passed \(limit) bytes; restarted]\n".utf8)
                    try out.write(contentsOf: marker)
                    written = marker.count
                    data = data.suffix(max(0, limit - written))
                }
                try out.write(contentsOf: data)
                written += data.count
            } catch {
                writable = false
            }
        }
    }
}
