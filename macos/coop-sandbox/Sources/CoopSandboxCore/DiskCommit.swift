import Foundation

/// Publishes a committed disk together with its metadata, the same way
/// ``DiskUpdate`` publishes a sandbox's disk with its record.
///
/// Publication (1) stages the metadata beside the inode of the prepared disk
/// in `disks/.pending-<name>.json`, (2) renames that disk over
/// `<name>.ext4`, and (3) writes `<name>.json` and drops the staged copy. The
/// rename is the commit point. After an interrupted publication, an installed
/// disk with the staged inode means step 3 must be finished; any other disk
/// (or none) means the commit never happened, so a replaced disk keeps its own
/// metadata and is never left without it. Holders of the disk's shared lock
/// read the staged metadata for a published disk (``Sandboxes/loadDiskMetadata(root:name:)``);
/// only an exclusive holder settles it.
enum DiskCommit {
    struct Pending: Codable {
        /// Basename of the prepared disk in `disks/`.
        var work: String
        var inode: UInt64
        var metadata: DiskMetadata
    }

    /// The points a publication can be interrupted at, for tests.
    enum Fault: CaseIterable, Sendable {
        case afterStaging, afterRename, afterMetadata
    }

    struct InjectedFault: Error {}

    static let pendingPrefix = ".pending-"

    static func pendingURL(_ root: SandboxRoot, _ name: SandboxID) -> URL {
        root.disks.appendingPathComponent("\(pendingPrefix)\(name.rawValue).json")
    }

    /// The disk a `disks/` entry stages a commit for, if it is a staged commit.
    static func diskName(pendingFile: String) -> SandboxID? {
        guard pendingFile.hasPrefix(pendingPrefix), pendingFile.hasSuffix(".json") else { return nil }
        return try? SandboxID(String(pendingFile.dropFirst(pendingPrefix.count).dropLast(5)))
    }

    /// Install `work` as committed disk `name`, described by `metadata`.
    /// Caller holds the disk's lock exclusively and has settled it.
    static func publish(_ root: SandboxRoot, _ name: SandboxID, work: URL, metadata: DiskMetadata, fault: Fault? = nil) throws {
        func inject(_ point: Fault) throws { if fault == point { throw InjectedFault() } }
        guard work.deletingLastPathComponent().standardizedFileURL == root.disks.standardizedFileURL else {
            throw SandboxError("prepared disk \(work.path) is not in \(root.disks.path)")
        }
        guard let inode = SandboxPaths.inode(work) else { throw SandboxError("stat \(work.path): errno \(errno)") }
        let pending = Pending(work: work.lastPathComponent, inode: inode, metadata: metadata)
        try JSONEncoder.pretty.encode(pending).write(to: pendingURL(root, name), options: .atomic)
        try inject(.afterStaging)
        guard rename(work.path, root.disk(name).path) == 0 else {
            let e = errno
            // A staged commit whose disk was not installed settles to nothing,
            // so a failed removal here loses nothing.
            try? FileManager.default.removeItem(at: pendingURL(root, name))
            throw SandboxError("rename \(work.lastPathComponent): errno \(e)")
        }
        try inject(.afterRename)
        try writeMetadata(root, name, metadata)
        try inject(.afterMetadata)
        try FileManager.default.removeItem(at: pendingURL(root, name))
    }

    /// Finish or discard an interrupted publication; `true` if there was one.
    /// Caller holds the disk's lock exclusively.
    @discardableResult
    static func settle(_ root: SandboxRoot, _ name: SandboxID) throws -> Bool {
        guard let pending = try loadPending(root, name) else { return false }
        if try isPublished(pending, root, name) {
            try writeMetadata(root, name, pending.metadata)
        } else if !pending.work.contains("/"), Sandboxes.isScratch(pending.work) {
            try? FileManager.default.removeItem(at: root.disks.appendingPathComponent(pending.work))
        }
        try FileManager.default.removeItem(at: pendingURL(root, name))
        return true
    }

    /// The staged commit, if any. One that cannot be read is an error:
    /// guessing could pair a disk with another commit's metadata.
    static func loadPending(_ root: SandboxRoot, _ name: SandboxID) throws -> Pending? {
        let url = pendingURL(root, name)
        let data: Data
        do {
            data = try Data(contentsOf: url)
        } catch CocoaError.fileReadNoSuchFile {
            return nil
        }
        do {
            return try JSONDecoder.iso.decode(Pending.self, from: data)
        } catch {
            throw SandboxError(
                "disk \(name) has an unreadable staged commit at \(url.path) (\(error)); remove it and commit the disk again")
        }
    }

    /// Whether `pending`'s disk is the installed one.
    static func isPublished(_ pending: Pending, _ root: SandboxRoot, _ name: SandboxID) throws -> Bool {
        guard let installed = SandboxPaths.inode(root.disk(name)) else { return false }
        if installed == pending.inode { return true }
        let work = root.disks.appendingPathComponent(pending.work)
        if let prepared = SandboxPaths.inode(work), prepared != pending.inode {
            throw SandboxError("disk \(name): scratch disk \(pending.work) is not the one its staged commit names")
        }
        return false
    }

    private static func writeMetadata(_ root: SandboxRoot, _ name: SandboxID, _ metadata: DiskMetadata) throws {
        try JSONEncoder.pretty.encode(metadata).write(to: Sandboxes.metadataURL(root: root, name: name), options: .atomic)
    }
}
