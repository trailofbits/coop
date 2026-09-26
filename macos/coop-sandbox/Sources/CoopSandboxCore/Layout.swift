import CryptoKit
import Foundation

/// Version of the JSON contract between coop and this binary. Bump on any
/// incompatible change to a command's arguments or output.
public let protocolVersion = 2
public let runtimeVersion = "0.2.0"
public let containerizationVersion = "0.45.0"

/// On-disk layout of one runtime state root. Everything the runtime owns
/// lives under `root`; nothing is read from or written to Apple Container's
/// stores.
public struct SandboxRoot: Sendable {
    public let root: URL

    /// `path` must be absolute. An existing root is canonicalized with
    /// realpath(3), so paths the runtime reports (e.g. the rootfs source in
    /// the effective config) compare exactly with the caller's canonical form.
    public init(_ path: String) throws {
        guard path.hasPrefix("/") else { throw SandboxError("--root must be an absolute path") }
        root = URL(fileURLWithPath: Self.canonical(path), isDirectory: true)
    }

    /// realpath(3) of `path`, or of its parent plus the last component when
    /// `path` does not exist yet, so a root reads the same before and after
    /// it is created.
    static func canonical(_ path: String) -> String {
        if let real = realpath(path, nil) {
            defer { free(real) }
            return String(cString: real)
        }
        let url = URL(fileURLWithPath: path)
        let parent = url.deletingLastPathComponent().path
        guard parent != path, parent != "/" || url.lastPathComponent != "" else { return path }
        return (canonical(parent) as NSString).appendingPathComponent(url.lastPathComponent)
    }

    public var imageStore: URL { root.appendingPathComponent("store", isDirectory: true) }
    public var kernel: URL { root.appendingPathComponent("vmlinux") }
    public var initfs: URL { root.appendingPathComponent("initfs.ext4") }
    public var sandboxes: URL { root.appendingPathComponent("sandboxes", isDirectory: true) }
    /// Unpacked image root filesystems, cloned into new sandboxes.
    public var bases: URL { root.appendingPathComponent("bases", isDirectory: true) }
    /// Disks saved by `commit`, cloned by `create --from-disk` and `restore`.
    public var disks: URL { root.appendingPathComponent("disks", isDirectory: true) }
    public var subnetState: URL { root.appendingPathComponent("subnets.json") }
    public var allocationLock: URL { root.appendingPathComponent("subnets.lock") }
    public var operationLock: URL { root.appendingPathComponent("operations.lock") }
    /// Lock files for sandboxes, committed disks, and the maintenance
    /// artifact (see ``FileLock``). They are never removed, so every process
    /// locks the same file for a given name, even across a delete.
    public var locks: URL { root.appendingPathComponent("locks", isDirectory: true) }
    /// The installed maintenance artifact (see ``MaintenanceArtifact``).
    public var maintenance: URL { root.appendingPathComponent("maintenance", isDirectory: true) }

    public func sandbox(_ id: SandboxID) -> SandboxPaths {
        SandboxPaths(id: id, dir: sandboxes.appendingPathComponent(id.rawValue, isDirectory: true), locks: locks)
    }

    /// Guards committed disk `name` and its metadata: exclusive to publish or
    /// delete them, shared to clone them.
    public func diskLock(_ name: SandboxID) -> URL {
        locks.appendingPathComponent("disk-\(name.rawValue).lock")
    }

    public func disk(_ name: SandboxID) -> URL {
        disks.appendingPathComponent("\(name.rawValue).ext4")
    }

    public func requireInitialized() throws {
        for url in [kernel, initfs, imageStore] where !FileManager.default.fileExists(atPath: url.path) {
            throw SandboxError("state root \(root.path) is not initialized; run `coop-sandbox init`")
        }
    }

    /// Every committed sandbox's `record.json`. A directory without a record
    /// is an uncommitted create and is skipped; a record that exists but
    /// cannot be read is an error, so listings and subnet allocation fail
    /// closed rather than forget a sandbox. A staged disk update is not
    /// applied: it never changes the identity, owner, or subnet these
    /// callers read, and one left unreadable must not hide every sandbox.
    public func allRecords() throws -> [SandboxRecord] {
        guard FileManager.default.fileExists(atPath: sandboxes.path) else { return [] }
        return try FileManager.default.contentsOfDirectory(atPath: sandboxes.path).sorted().compactMap { name in
            guard let id = try? SandboxID(name) else { return nil }
            let paths = sandbox(id)
            guard FileManager.default.fileExists(atPath: paths.record.path) else { return nil }
            return try paths.readRecordFile()
        }
    }

    /// Creates the state directories and makes each private: a real
    /// directory (not a symlink) owned by this user, mode 0700. One that
    /// already existed with a wider mode is narrowed; one owned by another
    /// user, or a symlink, is refused.
    public func createDirectories() throws {
        for dir in [root, sandboxes, bases, disks, locks, maintenance] {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try Self.makePrivate(dir)
        }
    }

    static func makePrivate(_ dir: URL) throws {
        var st = stat()
        guard lstat(dir.path, &st) == 0 else { throw SandboxError("lstat \(dir.path): errno \(errno)") }
        guard (st.st_mode & S_IFMT) == S_IFDIR, st.st_uid == getuid() else {
            throw SandboxError("state directory \(dir.path) is not a directory owned by this user")
        }
        if (st.st_mode & 0o7777) != 0o700, chmod(dir.path, 0o700) != 0 {
            throw SandboxError("chmod \(dir.path): errno \(errno)")
        }
    }
}

public struct SandboxPaths: Sendable {
    public let id: SandboxID
    public let dir: URL
    let locks: URL
    public var record: URL { dir.appendingPathComponent("record.json") }
    public var live: URL { dir.appendingPathComponent("live.json") }
    public var rootfs: URL { dir.appendingPathComponent("rootfs.ext4") }
    public var bootLog: URL { dir.appendingPathComponent("boot.log") }
    public var ownerLog: URL { dir.appendingPathComponent("owner.log") }
    public var lock: URL { dir.appendingPathComponent("owner.lock") }
    public var launchdPlist: URL { dir.appendingPathComponent("launchd.plist") }
    /// Written by an owner that failed before its VM ran; read by `start`.
    public var ownerFailed: URL { dir.appendingPathComponent("owner.failed") }
    /// A staged disk update (see ``DiskUpdate``).
    public var pendingDiskUpdate: URL { dir.appendingPathComponent("disk-update.pending.json") }
    /// A staged restore written by runtime 0.1.0, still recovered.
    var legacyPendingRestore: URL { dir.appendingPathComponent("restore.pending.json") }
    /// Serializes every mutation of this sandbox, and an owner's claim to
    /// it, across processes (see ``Sandboxes/mutating(_:settle:_:)``).
    public var mutationLock: URL { locks.appendingPathComponent("sandbox-\(id.rawValue).lock") }
    /// Held briefly: exclusive while a disk update's record is written,
    /// shared while the record is read, so a reader never pairs a record with
    /// the other side of a publication.
    var recordLock: URL { locks.appendingPathComponent("record-\(id.rawValue).lock") }
    /// Unix socket paths are limited to 104 bytes on macOS, so the control
    /// socket lives in a short per-user directory keyed by a hash of `dir`.
    public var control: URL {
        Self.controlDirectory.appendingPathComponent("\(Self.stableHash(dir.path)).sock")
    }

    /// `coop-sbx` in the per-user temporary directory, which is private to
    /// the user from boot: in shared `/tmp`, another user could pre-create
    /// the directory and block every sandbox. Read with confstr(3) because
    /// launchd jobs may not inherit `TMPDIR`.
    public static let controlDirectory: URL = {
        var buf = [CChar](repeating: 0, count: Int(PATH_MAX))
        let n = confstr(_CS_DARWIN_USER_TEMP_DIR, &buf, buf.count)
        let tmp = n > 0 && n <= buf.count ? String(cString: buf) : NSTemporaryDirectory()
        return URL(fileURLWithPath: tmp, isDirectory: true).appendingPathComponent("coop-sbx", isDirectory: true)
    }()
    /// launchd label, unique per state root and sandbox.
    public var launchdLabel: String { "dev.coop.sandbox.\(Self.stableHash(dir.path))" }

    static func stableHash(_ s: String) -> String {
        let h = s.utf8.reduce(UInt64(14_695_981_039_346_656_037)) { ($0 ^ UInt64($1)) &* 1_099_511_628_211 }
        return String(h, radix: 16)
    }

    /// The committed record. A disk update whose disk is already installed
    /// is committed even if a crash interrupted its record write, so its
    /// staged record is returned. Nothing is written here: only a guarded
    /// mutation settles the update (``DiskUpdate/settle(_:)``).
    public func loadRecord() throws -> SandboxRecord {
        let lock = try FileLock.acquire(recordLock, .shared)
        defer { withExtendedLifetime(lock) {} }
        let record = try readRecordFile()
        guard let (pending, _) = try DiskUpdate.loadPending(self) else { return record }
        return try DiskUpdate.isPublished(pending, self) ? pending.record : record
    }

    /// `record.json` as written, ignoring any staged disk update.
    func readRecordFile() throws -> SandboxRecord {
        try JSONDecoder.iso.decode(SandboxRecord.self, from: Data(contentsOf: record))
    }

    /// Callers hold the mutation guard, and no disk update is staged.
    public func save(_ record: SandboxRecord) throws {
        let lock = try FileLock.acquire(recordLock, .exclusive)
        defer { withExtendedLifetime(lock) {} }
        try writeRecordFile(record)
    }

    /// Caller holds `recordLock` exclusively.
    func writeRecordFile(_ record: SandboxRecord) throws {
        try JSONEncoder.pretty.encode(record).write(to: self.record, options: .atomic)
    }

    public func loadLive() -> LiveState? {
        guard let data = try? Data(contentsOf: live) else { return nil }
        return try? JSONDecoder.iso.decode(LiveState.self, from: data)
    }

    static func inode(_ url: URL) -> UInt64? {
        var st = stat()
        return lstat(url.path, &st) == 0 ? UInt64(st.st_ino) : nil
    }
}

/// A sandbox or disk identifier: always a safe single path component and a
/// valid container ID.
public struct SandboxID: RawRepresentable, Codable, Hashable, Sendable, CustomStringConvertible {
    public let rawValue: String
    public init?(rawValue: String) { try? self.init(rawValue) }
    public init(_ raw: String) throws {
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyz0123456789-")
        let ok = !raw.isEmpty && raw.count <= 48 && raw.first != "-"
            && raw.unicodeScalars.allSatisfy { allowed.contains($0) }
        guard ok else { throw SandboxError("invalid identifier \(raw.debugDescription)") }
        rawValue = raw
    }
    public init(from decoder: Decoder) throws {
        try self.init(try decoder.singleValueContainer().decode(String.self))
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(rawValue)
    }
    public var description: String { rawValue }
}

/// Identifies one `set`, `grow`, or `restore`. Callers pass their own to
/// correlate the outcome after a crash; otherwise one is generated.
public struct OperationID: RawRepresentable, Codable, Hashable, Sendable, CustomStringConvertible {
    public let rawValue: String
    public init?(rawValue: String) { try? self.init(rawValue) }
    public init(_ raw: String) throws {
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyz0123456789-")
        guard !raw.isEmpty, raw.count <= 64, raw.unicodeScalars.allSatisfy({ allowed.contains($0) }) else {
            throw SandboxError("invalid operation id \(raw.debugDescription)")
        }
        rawValue = raw
    }
    public static func random() -> OperationID {
        // A lowercased UUID always satisfies the rule above.
        OperationID(unchecked: UUID().uuidString.lowercased())
    }
    private init(unchecked: String) { rawValue = unchecked }
    public init(from decoder: Decoder) throws {
        try self.init(try decoder.singleValueContainer().decode(String.self))
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(rawValue)
    }
    public var description: String { rawValue }
}

/// Durable description of a persistent sandbox. Deliberately has no field
/// for host mounts, socket relays, published ports, or agent forwarding:
/// those states are not expressible.
public struct SandboxRecord: Codable, Sendable {
    public var id: SandboxID
    /// Caller-chosen ownership tag; `delete` refuses a mismatch.
    public var owner: String
    public var imageReference: String
    public var imageDigest: String
    /// Committed disk this sandbox was cloned from, if any.
    public var baseDisk: String?
    /// Init process environment, captured from the image config at create
    /// time so a start never depends on the image store.
    public var environment: [String]
    public var cpus: Int
    public var memoryBytes: UInt64
    public var diskBytes: UInt64
    public var subnetIndex: Int
    public var createdAt: Date
    /// Incremented each time `restore` replaces the disk, so a caller that
    /// crashed mid-restore can tell whether it applied.
    public var diskGeneration: Int = 0
    /// The last `set`, `grow`, or `restore` committed to this record, so a
    /// caller can tell whether its own operation applied and whether another
    /// has happened since. Absent until the first such operation.
    public var lastOperation: OperationID?

    public var subnet: String { Self.subnet(subnetIndex) }
    public static func subnet(_ index: Int) -> String { "10.231.\(index).0/24" }

    public init(
        id: SandboxID, owner: String, imageReference: String, imageDigest: String, baseDisk: String?,
        environment: [String], cpus: Int, memoryBytes: UInt64, diskBytes: UInt64, subnetIndex: Int, createdAt: Date
    ) {
        self.id = id
        self.owner = owner
        self.imageReference = imageReference
        self.imageDigest = imageDigest
        self.baseDisk = baseDisk
        self.environment = environment
        self.cpus = cpus
        self.memoryBytes = memoryBytes
        self.diskBytes = diskBytes
        self.subnetIndex = subnetIndex
        self.createdAt = createdAt
    }
}

/// Written by the owner process while the VM runs; removed on clean stop.
public struct LiveState: Codable, Sendable {
    public var pid: Int32
    public var startedAt: Date
    public var ipv4: String?
    public var ipv6: String?
}

public enum SandboxStatus: String, Codable, Sendable {
    case running, booting, stopped, crashed
}

public struct SandboxError: Error, CustomStringConvertible {
    public let description: String
    public init(_ message: String) { description = message }
}

/// Kernels this runtime has been validated with (sha256 of the vmlinux).
/// `init` refuses any other kernel; there is no override.
public enum KernelPin {
    public static let allowed: Set<String> = [
        // vmlinux-6.18.15-186, installed by Apple `container` 1.4.1 (Kata static build).
        "2fe4a58d2885d623bcb4d705900ac8c1d4f02371152da8126b3b00c8c47fc3a1"
    ]

    /// The kernel for `root`, validated against `allowed`. A root keeps a
    /// kernel it was initialized with while that kernel is still pinned, so a
    /// newer kernel installed elsewhere later does not break re-running init;
    /// otherwise `requested` replaces it. `install` is true when the kernel
    /// must be written.
    public static func select(
        root: SandboxRoot, requested: String, allowed: Set<String> = allowed
    ) throws -> (data: Data, sha256: String, install: Bool) {
        func pinned(_ path: String) throws -> (Data, String) {
            let data = try Data(contentsOf: URL(fileURLWithPath: path))
            let sha = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
            guard allowed.contains(sha) else {
                throw SandboxError("kernel \(path) (sha256 \(sha)) is not a validated kernel")
            }
            return (data, sha)
        }
        if let (data, sha) = try? pinned(root.kernel.path) { return (data, sha, false) }
        let (data, sha) = try pinned(requested)
        return (data, sha, true)
    }
}

/// An flock(2) lock on a file. The kernel drops it when the holder's
/// descriptor closes, including when the process dies, so a crashed holder
/// never leaves a stale lock.
///
/// Lock order: a holder may take only locks further down this list.
/// 1. `operations.lock` (``OperationLock``).
/// 2. `locks/sandbox-<id>.lock`, one sandbox's mutation guard
///    (``SandboxPaths/mutationLock``). An owner holds it only while it
///    claims ownership; a mutation never waits for `owner.lock`, it only
///    probes it.
/// 3. `locks/disk-<name>.lock`, one committed disk (``SandboxRoot/diskLock(_:)``).
/// 4. `subnets.lock`, the subnet allocator.
/// 5. Leaf locks, held briefly with nothing taken inside them:
///    `locks/record-<id>.lock` (``SandboxPaths/recordLock``) and
///    `locks/maintenance.lock` (``Maintenance``).
public final class FileLock {
    public enum Mode: Sendable {
        case shared, exclusive
        var operation: Int32 { self == .shared ? LOCK_SH : LOCK_EX }
    }

    private let fd: Int32

    private init(fd: Int32) { self.fd = fd }

    /// Blocks until the lock is granted.
    public static func acquire(_ url: URL, _ mode: Mode) throws -> FileLock {
        let fd = try openLockFile(url)
        while flock(fd, mode.operation) != 0 {
            let e = errno
            if e == EINTR { continue }
            close(fd)
            throw SandboxError("flock \(url.lastPathComponent): errno \(e)")
        }
        return FileLock(fd: fd)
    }

    /// Waits without blocking a thread, for callers on Swift concurrency.
    public static func acquire(_ url: URL, _ mode: Mode, polling interval: Duration) async throws -> FileLock {
        while true {
            if let lock = try attempt(url, mode) { return lock }
            try await Task.sleep(for: interval)
        }
    }

    /// `nil` while a conflicting holder has it.
    public static func attempt(_ url: URL, _ mode: Mode) throws -> FileLock? {
        let fd = try openLockFile(url)
        guard flock(fd, mode.operation | LOCK_NB) == 0 else {
            let e = errno
            close(fd)
            if e == EWOULDBLOCK { return nil }
            throw SandboxError("flock \(url.lastPathComponent): errno \(e)")
        }
        return FileLock(fd: fd)
    }

    /// Creates the lock directory inside an existing state root, never the
    /// root itself.
    static func openLockFile(_ url: URL) throws -> Int32 {
        let dir = url.deletingLastPathComponent()
        if mkdir(dir.path, 0o700) != 0, errno != EEXIST {
            throw SandboxError("mkdir \(dir.path): errno \(errno)")
        }
        let fd = open(url.path, O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard fd >= 0 else { throw SandboxError("open \(url.path): errno \(errno)") }
        return fd
    }

    deinit { close(fd) }
}

/// Serializes offline operations against `reconcile`'s sweep of scratch
/// files and uncommitted creates. Operations hold it shared (distinct
/// sandboxes run concurrently; ``Sandboxes/mutating(_:settle:_:)``
/// serializes each one); the sweep needs it exclusively.
public enum OperationLock {
    /// Blocks until no sweep is running.
    public static func shared(_ root: SandboxRoot) throws -> FileLock {
        try FileLock.acquire(root.operationLock, .shared)
    }

    /// `nil` while any operation is in progress.
    public static func tryExclusive(_ root: SandboxRoot) -> FileLock? {
        (try? FileLock.attempt(root.operationLock, .exclusive)) ?? nil
    }
}

/// Clones `from` to `to`, owner-only: clonefile(2) copies the source's
/// mode, which the process umask does not narrow.
public func clone(_ from: URL, to: URL) throws {
    guard clonefile(from.path, to.path, 0) == 0 else {
        throw SandboxError("clonefile \(from.lastPathComponent) -> \(to.lastPathComponent): errno \(errno)")
    }
    guard chmod(to.path, 0o600) == 0 else { throw SandboxError("chmod \(to.lastPathComponent): errno \(errno)") }
}

public func printJSON<T: Encodable>(_ value: T) throws {
    FileHandle.standardOutput.write(try JSONEncoder.pretty.encode(value))
    FileHandle.standardOutput.write(Data("\n".utf8))
}

extension JSONEncoder {
    public static var pretty: JSONEncoder {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        e.dateEncodingStrategy = .iso8601
        return e
    }
}

extension JSONDecoder {
    public static var iso: JSONDecoder {
        let d = JSONDecoder()
        d.dateDecodingStrategy = .iso8601
        return d
    }
}
