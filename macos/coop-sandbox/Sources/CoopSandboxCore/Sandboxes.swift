import Containerization
import Foundation

/// What `create` clones a new sandbox from.
public enum SandboxSource: Sendable {
    case image(String)
    case disk(SandboxID)
}

/// Metadata saved beside a committed disk; a sandbox created from it
/// inherits these fields.
public struct DiskMetadata: Codable, Sendable {
    public var imageReference: String
    public var imageDigest: String
    public var environment: [String]
    public var diskBytes: UInt64
    public var committedFrom: SandboxID
    public var createdAt: Date
}

public struct InspectOutput: Encodable, Sendable {
    public var record: SandboxRecord
    public var status: SandboxStatus
    public var live: LiveState?
    public var effective: EffectiveConfig?
    public var disk: DiskSummary
}

public struct ReconcileAction: Codable, Sendable {
    public var id: String
    public var status: String
    public var action: String
}

/// Sandbox lifecycle operations. Every mutation of a sandbox's disk or
/// record runs under its mutation guard and requires it to be stopped with
/// no live owner.
public enum Sandboxes {
    // MARK: status

    public static func status(_ paths: SandboxPaths) -> SandboxStatus {
        guard let live = paths.loadLive() else {
            return ownerHoldsLock(paths) ? .booting : .stopped
        }
        // The owner holds its lock for its whole life, so the lock (not the
        // recorded PID, which the system may reuse) proves it is alive.
        guard ownerHoldsLock(paths) else { return .crashed }
        return (try? ControlSocket.call(paths.control, .init(op: .ping), timeout: 5))?.ok == true ? .running : .booting
    }

    /// Whether some process holds the owner lock (an owner between launch
    /// and writing live.json, or a wedged one). Only a missing lock file
    /// means no owner; a lock that cannot be probed counts as held, so a
    /// sandbox is never taken for stopped on an error.
    static func ownerHoldsLock(_ paths: SandboxPaths) -> Bool {
        let fd = open(paths.lock.path, O_RDWR | O_CLOEXEC)
        guard fd >= 0 else { return errno != ENOENT }
        defer { close(fd) }
        if flock(fd, LOCK_EX | LOCK_NB) == 0 {
            flock(fd, LOCK_UN)
            return false
        }
        return true
    }

    /// Why the owner lock cannot be probed, or nil when it can (or is
    /// absent). Such a sandbox reads as held forever, so callers report this
    /// cause instead of a status the lock never proved.
    static func lockProbeFailure(_ paths: SandboxPaths) -> String? {
        let fd = open(paths.lock.path, O_RDWR | O_CLOEXEC)
        guard fd < 0 else {
            close(fd)
            return nil
        }
        let code = errno
        guard code != ENOENT else { return nil }
        return "cannot open its owner lock \(paths.lock.path): \(String(cString: strerror(code)))"
    }

    public static func requireStopped(_ paths: SandboxPaths, _ id: SandboxID) throws {
        if let why = lockProbeFailure(paths) { throw SandboxError("\(id) state is unknown: \(why)") }
        let s = status(paths)
        guard s == .stopped else { throw SandboxError("\(id) is \(s.rawValue); stop it first") }
    }

    /// Runs `body` holding `paths`' mutation guard, so no other mutation of
    /// the sandbox and no owner (started by `start` or respawned by launchd)
    /// can interleave with it; see ``Owner/claim(root:id:)``. Unless `settle`
    /// is false, a disk update a crash interrupted is finished or discarded
    /// first. Checks that the sandbox is stopped belong inside `body`, so the
    /// whole check-and-mutate interval is guarded.
    static func mutating<T>(_ paths: SandboxPaths, settle: Bool = true, _ body: () async throws -> T) async throws -> T {
        let guarded = try await FileLock.acquire(paths.mutationLock, .exclusive, polling: .milliseconds(50))
        defer { withExtendedLifetime(guarded) {} }
        if settle { try DiskUpdate.settle(paths) }
        return try await body()
    }

    /// Runs `body` holding committed disk `name`'s lock.
    static func withDisk<T>(_ root: SandboxRoot, _ name: SandboxID, _ mode: FileLock.Mode, _ body: () throws -> T) async throws -> T {
        let lock = try await FileLock.acquire(root.diskLock(name), mode, polling: .milliseconds(50))
        defer { withExtendedLifetime(lock) {} }
        return try body()
    }

    // MARK: create

    public static func create(
        root: SandboxRoot, id: SandboxID, owner: String, source: SandboxSource, cpus: Int, memoryBytes: UInt64, diskBytes: UInt64
    ) async throws -> SandboxRecord {
        let operation = try OperationLock.shared(root)
        defer { withExtendedLifetime(operation) {} }
        try root.requireInitialized()
        guard cpus >= 1, memoryBytes >= 256 * 1024 * 1024, diskBytes >= 1024 * 1024 * 1024 else {
            throw SandboxError("cpus must be >= 1, memory >= 256 MiB, disk >= 1 GiB")
        }
        let paths = root.sandbox(id)
        return try await mutating(paths, settle: false) {
            // No record yet means an uncommitted create; reconcile removes those.
            try FileManager.default.createDirectory(at: paths.dir, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
            do {
                return try await populate(
                    root: root, paths: paths, id: id, owner: owner, source: source, cpus: cpus, memoryBytes: memoryBytes, diskBytes: diskBytes)
            } catch {
                try? FileManager.default.removeItem(at: paths.dir)
                throw error
            }
        }
    }

    static func populate(
        root: SandboxRoot, paths: SandboxPaths, id: SandboxID, owner: String, source: SandboxSource, cpus: Int, memoryBytes: UInt64,
        diskBytes: UInt64
    ) async throws -> SandboxRecord {

        let imageReference: String
        let imageDigest: String
        let environment: [String]
        var baseDisk: String?
        switch source {
        case .image(let reference):
            let store = try ImageStore(path: root.imageStore)
            let image = try await store.get(reference: reference)
            let base = try await Disks.base(root: root, image: image, bytes: diskBytes)
            try clone(base, to: paths.rootfs)
            imageReference = reference
            imageDigest = image.digest
            environment = try await image.config(for: .current).config?.env ?? ["PATH=\(LinuxProcessConfiguration.defaultPath)"]
        case .disk(let name):
            let meta = try await cloneCommitted(root: root, name: name, to: paths.rootfs)
            guard diskBytes >= meta.diskBytes else {
                throw SandboxError("disk \(name) is \(meta.diskBytes) bytes; cannot create a smaller sandbox from it")
            }
            // Nothing refers to this disk until the record is written, so it
            // is grown in place.
            if diskBytes > meta.diskBytes {
                try await growScratch(root: root, scratch: paths.dir, disk: paths.rootfs, to: diskBytes)
            }
            imageReference = meta.imageReference
            imageDigest = meta.imageDigest
            environment = meta.environment
            baseDisk = name.rawValue
        }

        var record: SandboxRecord?
        try SubnetAllocator(root: root).allocate(for: id) { index in
            let r = SandboxRecord(
                id: id, owner: owner, imageReference: imageReference, imageDigest: imageDigest, baseDisk: baseDisk,
                environment: environment, cpus: cpus, memoryBytes: memoryBytes, diskBytes: diskBytes, subnetIndex: index,
                createdAt: Date())
            // Writing the record commits the create.
            try paths.save(r)
            record = r
        }
        guard let record else { throw SandboxError("allocation did not commit") }
        return record
    }

    /// Clone committed disk `name` to `url` and return its metadata, both
    /// from one publication of that name.
    static func cloneCommitted(root: SandboxRoot, name: SandboxID, to url: URL) async throws -> DiskMetadata {
        try await withDisk(root, name, .shared) {
            let meta = try loadDiskMetadata(root: root, name: name)
            try clone(root.disk(name), to: url)
            return meta
        }
    }

    // MARK: start / stop

    public static func start(root: SandboxRoot, id: SandboxID, executable: String, wait: TimeInterval) async throws -> LiveState {
        try root.requireInitialized()
        let paths = root.sandbox(id)
        // Guarded up to the bootstrap only: the owner takes the guard to
        // claim the sandbox, so waiting for it here would deadlock.
        try await mutating(paths) {
            _ = try paths.loadRecord()
            if status(paths) == .crashed {
                // Take the job back from launchd before starting it fresh.
                Launchd.bootout(paths.launchdLabel)
                try? FileManager.default.removeItem(at: paths.live)
            }
            try requireStopped(paths, id)
            Launchd.bootout(paths.launchdLabel)
            let domain = Launchd.domain()
            let plist = Launchd.plist(
                label: paths.launchdLabel, executable: executable,
                arguments: ["run", id.rawValue, "--root", root.root.path], log: paths.ownerLog)
            try Launchd.write(plist, to: paths.launchdPlist)
            try clearOwnerFailure(paths)
            try Launchd.bootstrap(plist: paths.launchdPlist, domain: domain)
        }

        let deadline = Date().addingTimeInterval(wait)
        while Date() < deadline {
            if let live = paths.loadLive(), (try? ControlSocket.call(paths.control, .init(op: .ping), timeout: 5))?.ok == true {
                return live
            }
            if let failure = try? String(contentsOf: paths.ownerFailed, encoding: .utf8) {
                Launchd.bootout(paths.launchdLabel)
                throw SandboxError("\(id) failed to start: \(failure)")
            }
            try await Task.sleep(for: .milliseconds(100))
        }
        let tail = logTail(paths.ownerLog, lines: 20)
        Launchd.bootout(paths.launchdLabel)
        throw SandboxError("\(id) did not become ready within \(Int(wait))s; owner log:\n\(tail)")
    }

    /// Remove the previous owner's failure marker, so `start` never reports
    /// it as this start's error.
    static func clearOwnerFailure(_ paths: SandboxPaths) throws {
        guard unlink(paths.ownerFailed.path) == 0 || errno == ENOENT else {
            throw SandboxError("remove \(paths.ownerFailed.path): errno \(errno)")
        }
    }

    /// Halt systemd cleanly, wait for the owner to exit, and unload its job.
    /// Idempotent: stopping a stopped sandbox succeeds. Not guarded: it
    /// changes no disk or record, and must work while an owner is starting.
    public static func stop(root: SandboxRoot, id: SandboxID, timeout: TimeInterval) async throws {
        let paths = root.sandbox(id)
        _ = try paths.loadRecord()
        if let why = lockProbeFailure(paths) {
            // An owner may still be running; unload its job before reporting
            // that the stop cannot be confirmed.
            Launchd.bootout(paths.launchdLabel)
            throw SandboxError("\(id) stop cannot be confirmed: \(why)")
        }
        if status(paths) == .running {
            _ = try? ControlSocket.call(paths.control, .init(op: .stop), timeout: timeout)
        }
        let deadline = Date().addingTimeInterval(timeout)
        while ownerHoldsLock(paths), Date() < deadline {
            try await Task.sleep(for: .milliseconds(100))
        }
        // bootout also covers an owner that ignored the request: launchd
        // sends SIGTERM (a graceful halt) and SIGKILL after ExitTimeOut.
        Launchd.bootout(paths.launchdLabel)
        let hard = Date().addingTimeInterval(100)
        while ownerHoldsLock(paths), Date() < hard { try await Task.sleep(for: .milliseconds(100)) }
        guard !ownerHoldsLock(paths) else { throw SandboxError("\(id) owner did not exit") }
        try? FileManager.default.removeItem(at: paths.live)
    }

    // MARK: inspect

    public static func inspect(root: SandboxRoot, id: SandboxID) throws -> InspectOutput {
        let paths = root.sandbox(id)
        let record = try paths.loadRecord()
        let s = status(paths)
        let effective = s == .running ? try? ControlSocket.call(paths.control, .init(op: .inspect), timeout: 10).inspect : nil
        let (logical, allocated) = Disks.sizes(paths.rootfs)
        return InspectOutput(
            record: record, status: s, live: s == .stopped ? nil : paths.loadLive(), effective: effective,
            disk: DiskSummary(name: "rootfs", logicalBytes: logical, allocatedBytes: allocated))
    }

    // MARK: resources and disks

    /// Change CPU/memory, applied at the next start. With `expect`, refuses
    /// unless the record's last operation is still `expect`, so a caller
    /// undoing its own change never overwrites a newer one.
    public static func setResources(
        root: SandboxRoot, id: SandboxID, cpus: Int?, memoryBytes: UInt64?, operation: OperationID? = nil, expect: OperationID? = nil
    ) async throws -> SandboxRecord {
        let lock = try OperationLock.shared(root)
        defer { withExtendedLifetime(lock) {} }
        let paths = root.sandbox(id)
        return try await mutating(paths) {
            try requireStopped(paths, id)
            var record = try paths.loadRecord()
            if let expect, record.lastOperation != expect {
                throw SandboxError(
                    "\(id) was changed by operation \(record.lastOperation?.rawValue ?? "(none)") after \(expect); refusing to overwrite it")
            }
            if let cpus {
                guard cpus >= 1 else { throw SandboxError("cpus must be >= 1") }
                record.cpus = cpus
            }
            if let memoryBytes {
                guard memoryBytes >= 256 * 1024 * 1024 else { throw SandboxError("memory must be >= 256 MiB") }
                record.memoryBytes = memoryBytes
            }
            record.lastOperation = operation ?? .random()
            try paths.save(record)
            return record
        }
    }

    public static func grow(root: SandboxRoot, id: SandboxID, diskBytes: UInt64, operation: OperationID? = nil) async throws -> SandboxRecord {
        let lock = try OperationLock.shared(root)
        defer { withExtendedLifetime(lock) {} }
        let paths = root.sandbox(id)
        return try await mutating(paths) {
            try requireStopped(paths, id)
            var record = try paths.loadRecord()
            guard diskBytes > record.diskBytes else { throw SandboxError("shrinking a disk is not supported") }
            let op = operation ?? .random()
            let work = DiskUpdate.workDisk(paths, op)
            try? FileManager.default.removeItem(at: work)
            defer { try? FileManager.default.removeItem(at: work) }
            try clone(paths.rootfs, to: work)
            try await growScratch(root: root, scratch: paths.dir, disk: work, to: diskBytes)
            record.diskBytes = diskBytes
            record.lastOperation = op
            try DiskUpdate.publish(paths, work: work, record: record)
            return record
        }
    }

    /// Enlarge `disk`, a file nothing else uses yet, and grow its filesystem.
    static func growScratch(root: SandboxRoot, scratch: URL, disk: URL, to bytes: UInt64) async throws {
        guard truncate(disk.path, off_t(bytes)) == 0 else { throw SandboxError("truncate: errno \(errno)") }
        let log = try await Maintenance.growFilesystem(root: root, scratch: scratch, disk: disk)
        Owner.log("grew \(disk.lastPathComponent) to \(bytes) bytes: \(log.split(separator: "\n").last ?? "")")
    }

    /// Save a stopped sandbox's disk as `name`, with its identity removed.
    public static func commit(root: SandboxRoot, id: SandboxID, name: SandboxID, replace: Bool) async throws -> DiskSummary {
        let lock = try OperationLock.shared(root)
        defer { withExtendedLifetime(lock) {} }
        let paths = root.sandbox(id)
        return try await mutating(paths) {
            try requireStopped(paths, id)
            let record = try paths.loadRecord()
            let target = root.disk(name)
            if FileManager.default.fileExists(atPath: target.path), !replace {
                throw SandboxError("disk \(name) exists")
            }
            let tmp = root.disks.appendingPathComponent(".tmp-\(name.rawValue)-\(UUID().uuidString.prefix(8)).ext4")
            defer { try? FileManager.default.removeItem(at: tmp) }
            try clone(paths.rootfs, to: tmp)
            // The reset runs coop-built tools against the disk as data, so the
            // guest's own binaries cannot interfere with it. It removes only the
            // identity left on disk: a guest that authored the disk can still
            // re-create its old identity when a clone boots.
            _ = try await Maintenance.resetIdentity(root: root, scratch: root.disks, disk: tmp)
            let meta = DiskMetadata(
                imageReference: record.imageReference, imageDigest: record.imageDigest, environment: record.environment,
                diskBytes: record.diskBytes, committedFrom: id, createdAt: Date())
            return try await withDisk(root, name, .exclusive) {
                try DiskCommit.settle(root, name)
                if FileManager.default.fileExists(atPath: target.path), !replace {
                    throw SandboxError("disk \(name) exists")
                }
                try DiskCommit.publish(root, name, work: tmp, metadata: meta)
                let (logical, allocated) = Disks.sizes(target)
                return DiskSummary(name: name.rawValue, logicalBytes: logical, allocatedBytes: allocated)
            }
        }
    }

    /// Replace a stopped sandbox's disk with a clone of committed disk `name`
    /// or a fresh copy of an image, grown back to the sandbox's size if that
    /// is larger. The new disk has no host keys, so the next boot generates
    /// new ones.
    public static func restore(root: SandboxRoot, id: SandboxID, source: SandboxSource, operation: OperationID? = nil) async throws
        -> SandboxRecord
    {
        let lock = try OperationLock.shared(root)
        defer { withExtendedLifetime(lock) {} }
        let paths = root.sandbox(id)
        return try await mutating(paths) {
            try requireStopped(paths, id)
            var record = try paths.loadRecord()
            let op = operation ?? .random()
            let work = DiskUpdate.workDisk(paths, op)
            try? FileManager.default.removeItem(at: work)
            defer { try? FileManager.default.removeItem(at: work) }
            let sourceBytes: UInt64
            switch source {
            case .disk(let name):
                let meta = try await cloneCommitted(root: root, name: name, to: work)
                sourceBytes = meta.diskBytes
                record.imageReference = meta.imageReference
                record.imageDigest = meta.imageDigest
                record.environment = meta.environment
                record.baseDisk = name.rawValue
            case .image(let reference):
                let store = try ImageStore(path: root.imageStore)
                let image = try await store.get(reference: reference)
                try clone(try await Disks.base(root: root, image: image, bytes: record.diskBytes), to: work)
                sourceBytes = record.diskBytes
                record.imageReference = reference
                record.imageDigest = image.digest
                record.environment = try await image.config(for: .current).config?.env ?? ["PATH=\(LinuxProcessConfiguration.defaultPath)"]
                record.baseDisk = nil
            }
            if record.diskBytes > sourceBytes {
                try await growScratch(root: root, scratch: paths.dir, disk: work, to: record.diskBytes)
            } else {
                record.diskBytes = sourceBytes
            }
            record.diskGeneration += 1
            record.lastOperation = op
            try DiskUpdate.publish(paths, work: work, record: record)
            return record
        }
    }

    public static func deleteDisk(root: SandboxRoot, name: SandboxID) async throws {
        let lock = try OperationLock.shared(root)
        defer { withExtendedLifetime(lock) {} }
        try await withDisk(root, name, .exclusive) {
            try DiskCommit.settle(root, name)
            try FileManager.default.removeItem(at: root.disk(name))
            try? FileManager.default.removeItem(at: metadataURL(root: root, name: name))
        }
    }

    static func metadataURL(root: SandboxRoot, name: SandboxID) -> URL {
        root.disks.appendingPathComponent("\(name.rawValue).json")
    }

    /// Caller holds `name`'s disk lock, shared or exclusive.
    static func loadDiskMetadata(root: SandboxRoot, name: SandboxID) throws -> DiskMetadata {
        guard FileManager.default.fileExists(atPath: root.disk(name).path) else { throw SandboxError("no disk named \(name)") }
        if let pending = try DiskCommit.loadPending(root, name), try DiskCommit.isPublished(pending, root, name) {
            return pending.metadata
        }
        return try JSONDecoder.iso.decode(DiskMetadata.self, from: Data(contentsOf: metadataURL(root: root, name: name)))
    }

    // MARK: delete / reconcile

    /// Does not settle a staged disk update first: deleting must stay
    /// possible even when that state is unreadable.
    public static func delete(root: SandboxRoot, id: SandboxID, owner: String) async throws {
        let lock = try OperationLock.shared(root)
        defer { withExtendedLifetime(lock) {} }
        let paths = root.sandbox(id)
        try await mutating(paths, settle: false) {
            try requireStopped(paths, id)
            let record = try paths.readRecordFile()
            guard record.owner == owner else { throw SandboxError("\(id) belongs to owner \(record.owner), not \(owner)") }
            Launchd.bootout(paths.launchdLabel)
            // Rename first so an interrupted delete never leaves a half-removed
            // sandbox that still looks valid; reconcile finishes the removal.
            let tomb = root.sandboxes.appendingPathComponent(".deleting-\(id.rawValue)-\(UUID().uuidString.prefix(8))")
            guard rename(paths.dir.path, tomb.path) == 0 else { throw SandboxError("rename: errno \(errno)") }
            try FileManager.default.removeItem(at: tomb)
        }
    }

    /// Clear crashed owners' state, finish interrupted disk updates and
    /// deletes, and remove uncommitted creates and leftover scratch files. A
    /// sandbox whose guard is held is left for a later run.
    public static func reconcile(root: SandboxRoot) throws -> [ReconcileAction] {
        var out: [ReconcileAction] = []
        // The sweep below must not race an in-flight offline operation; while
        // one runs, only per-sandbox recovery (crashed owners, staged disk
        // updates) happens.
        let sweep = OperationLock.tryExclusive(root)
        defer { withExtendedLifetime(sweep) {} }
        for record in try root.allRecords() {
            let paths = root.sandbox(record.id)
            guard let guarded = try FileLock.attempt(paths.mutationLock, .exclusive) else {
                out.append(.init(id: record.id.rawValue, status: "busy", action: "skipped-mutation-in-progress"))
                continue
            }
            let s = status(paths)
            var actions: [String] = []
            if s == .crashed {
                Launchd.bootout(paths.launchdLabel)
                try? FileManager.default.removeItem(at: paths.live)
                unlink(paths.control.path)
                actions.append("cleared-crashed-owner")
            }
            if s == .stopped || s == .crashed {
                do {
                    if try DiskUpdate.settle(paths) { actions.append("settled-disk-update") }
                } catch {
                    actions.append("unresolved-disk-update: \(error)")
                }
            }
            withExtendedLifetime(guarded) {}
            out.append(.init(id: record.id.rawValue, status: s.rawValue, action: actions.isEmpty ? "none" : actions.joined(separator: ",")))
        }
        guard sweep != nil else {
            out.append(.init(id: "-", status: "busy", action: "sweep-skipped-operation-in-progress"))
            return out
        }
        // Before the scratch sweep, which removes an unpublished commit's
        // disk, and the orphan sweep, which would remove a published one.
        var unresolvedCommits = Set<String>()
        for name in (try? FileManager.default.contentsOfDirectory(atPath: root.disks.path).sorted()) ?? [] {
            guard let disk = DiskCommit.diskName(pendingFile: name) else { continue }
            do {
                guard let lock = try FileLock.attempt(root.diskLock(disk), .exclusive) else {
                    unresolvedCommits.insert(disk.rawValue)
                    continue
                }
                defer { withExtendedLifetime(lock) {} }
                if try DiskCommit.settle(root, disk) {
                    out.append(.init(id: disk.rawValue, status: "committing", action: "settled-disk-commit"))
                }
            } catch {
                unresolvedCommits.insert(disk.rawValue)
                out.append(.init(id: disk.rawValue, status: "committing", action: "unresolved-disk-commit: \(error)"))
            }
        }
        let names = (try? FileManager.default.contentsOfDirectory(atPath: root.sandboxes.path)) ?? []
        for name in names.sorted() {
            let dir = root.sandboxes.appendingPathComponent(name)
            if name.hasPrefix(".deleting-") {
                try FileManager.default.removeItem(at: dir)
                out.append(.init(id: name, status: "deleting", action: "finished-delete"))
            } else if let id = try? SandboxID(name), !FileManager.default.fileExists(atPath: root.sandbox(id).record.path),
                !ownerHoldsLock(root.sandbox(id))
            {
                try FileManager.default.removeItem(at: dir)
                out.append(.init(id: name, status: "incomplete", action: "removed-uncommitted-create"))
            }
        }
        var scratchDirs = [root.bases, root.disks, root.maintenance]
        scratchDirs += try root.allRecords().map { root.sandbox($0.id).dir }
        for dir in scratchDirs {
            // Scratch files come only from offline operations on a stopped
            // sandbox; any other sandbox is left for a later sweep, as is one
            // whose staged disk update is still unresolved.
            if dir.deletingLastPathComponent() == root.sandboxes, let id = try? SandboxID(dir.lastPathComponent) {
                let paths = root.sandbox(id)
                let unresolved: Bool
                do { unresolved = try DiskUpdate.loadPending(paths) != nil } catch { unresolved = true }
                if status(paths) != .stopped || unresolved { continue }
            }
            for name in (try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []
            where isScratch(name) {
                try? FileManager.default.removeItem(at: dir.appendingPathComponent(name))
            }
        }
        for name in (try? FileManager.default.contentsOfDirectory(atPath: root.disks.path)) ?? []
        where name.hasSuffix(".ext4") && !name.hasPrefix(".") {
            let disk = root.disks.appendingPathComponent(name)
            if unresolvedCommits.contains(String(name.dropLast(5))) { continue }
            if !FileManager.default.fileExists(atPath: disk.deletingPathExtension().appendingPathExtension("json").path) {
                try? FileManager.default.removeItem(at: disk)
                out.append(.init(id: name, status: "incomplete", action: "removed-uncommitted-disk"))
            }
        }
        return out
    }

    /// Scratch files left by an interrupted offline operation.
    static func isScratch(_ name: String) -> Bool {
        [".tmp-", ".update-", ".grow-", ".restore-", ".maintenance-"].contains { name.hasPrefix($0) }
    }

    // MARK: logs

    /// The last `lines` lines of `url`. Splits on bytes: the serial console
    /// writes CRLF, which Swift's `String` treats as a single character.
    public static func logTail(_ url: URL, lines: Int) -> String {
        String(decoding: tailData(url, lines: lines), as: UTF8.self)
    }

    /// The last `lines` lines of `url`, reading at most its last 256 KiB.
    public static func tailData(_ url: URL, lines: Int, window: UInt64 = 256 * 1024) -> Data {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return Data() }
        defer { try? handle.close() }
        guard let end = try? handle.seekToEnd() else { return Data() }
        try? handle.seek(toOffset: end > window ? end - window : 0)
        var data = (try? handle.readToEnd()) ?? Data()
        // A window that starts mid-line would return a fragment as a line.
        if end > window, let nl = data.firstIndex(of: 0x0A) { data = data[(nl + 1)...] }
        return tailLines(Data(data), lines)
    }

    public static func tailLines(_ data: Data, _ lines: Int) -> Data {
        var parts = data.split(separator: 0x0A, omittingEmptySubsequences: false)
        if parts.last?.isEmpty == true { parts.removeLast() }
        let kept = parts.suffix(lines)
        guard !kept.isEmpty else { return Data() }
        return Data(kept.joined(separator: [0x0A])) + [0x0A]
    }
}
