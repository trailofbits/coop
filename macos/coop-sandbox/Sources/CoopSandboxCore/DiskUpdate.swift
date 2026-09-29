import Foundation

/// The one way a sandbox's disk is replaced after create: `grow` and
/// `restore` both prepare a scratch disk and publish it here, together with
/// the record that describes it.
///
/// Everything that can fail runs on the scratch disk first. Publication then
/// (1) stages the new record beside the inode of the prepared disk, (2)
/// renames that disk over `rootfs.ext4`, and (3) writes the record and drops
/// the staged copy. The rename is the commit point. After an interrupted
/// publication, an installed disk with the staged inode means step 3 must be
/// finished; any other disk means the update never happened, and its scratch
/// disk is discarded. So disk and record always describe the same update.
///
/// This recovers deterministically from a process crash at any step. It
/// makes no claim about sudden power loss: each rename is atomic, but the
/// steps are not synced to disk as a group.
public enum DiskUpdate {
    struct Pending: Codable {
        /// Basename of the prepared disk in the sandbox directory.
        var work: String
        var inode: UInt64
        var record: SandboxRecord
    }

    /// The four points a publication can be interrupted at, for tests.
    enum Fault: CaseIterable, Sendable {
        case beforeStaging, afterStaging, afterRename, afterRecord
    }

    struct InjectedFault: Error {}

    /// The scratch disk an update prepares.
    public static func workDisk(_ paths: SandboxPaths, _ operation: OperationID) -> URL {
        paths.dir.appendingPathComponent(".update-\(operation.rawValue).ext4")
    }

    /// Install the prepared disk `work` as `paths`' disk, described by
    /// `record`. Caller holds the sandbox's mutation guard.
    static func publish(_ paths: SandboxPaths, work: URL, record: SandboxRecord, fault: Fault? = nil) throws {
        func inject(_ point: Fault) throws { if fault == point { throw InjectedFault() } }
        guard work.deletingLastPathComponent().standardizedFileURL == paths.dir.standardizedFileURL else {
            throw SandboxError("prepared disk \(work.path) is not in \(paths.dir.path)")
        }
        try inject(.beforeStaging)
        guard let inode = SandboxPaths.inode(work) else { throw SandboxError("stat \(work.path): errno \(errno)") }
        let pending = Pending(work: work.lastPathComponent, inode: inode, record: record)
        try JSONEncoder.pretty.encode(pending).write(to: paths.pendingDiskUpdate, options: .atomic)
        try inject(.afterStaging)
        guard rename(work.path, paths.rootfs.path) == 0 else {
            let e = errno
            try? FileManager.default.removeItem(at: paths.pendingDiskUpdate)
            throw SandboxError("rename \(work.lastPathComponent): errno \(e)")
        }
        try inject(.afterRename)
        let lock = try FileLock.acquire(paths.recordLock, .exclusive)
        defer { withExtendedLifetime(lock) {} }
        try paths.writeRecordFile(record)
        try inject(.afterRecord)
        try FileManager.default.removeItem(at: paths.pendingDiskUpdate)
    }

    /// Finish or discard an interrupted publication; `true` if there was one.
    /// Caller holds the sandbox's mutation guard.
    @discardableResult
    static func settle(_ paths: SandboxPaths) throws -> Bool {
        guard let (pending, file) = try loadPending(paths) else { return false }
        let published = try isPublished(pending, paths)
        let lock = try FileLock.acquire(paths.recordLock, .exclusive)
        defer { withExtendedLifetime(lock) {} }
        if published {
            try paths.writeRecordFile(pending.record)
        } else {
            try? FileManager.default.removeItem(at: paths.dir.appendingPathComponent(pending.work))
        }
        try FileManager.default.removeItem(at: file)
        return true
    }

    /// The staged update and the file it came from, if any. State that
    /// cannot be read, or that contradicts itself, is an error: guessing
    /// could pair a disk with another update's record.
    static func loadPending(_ paths: SandboxPaths) throws -> (Pending, URL)? {
        let current = try read(paths.pendingDiskUpdate)
        let legacy = try read(paths.legacyPendingRestore)
        func unreadable(_ file: URL, _ error: Error) -> SandboxError {
            SandboxError(
                "\(paths.id) has an unreadable staged disk update at \(file.path) (\(error)); compare rootfs.ext4 with "
                    + "record.json by hand before removing it")
        }
        switch (current, legacy) {
        case (nil, nil):
            return nil
        case (let data?, nil):
            do {
                return (try JSONDecoder.iso.decode(Pending.self, from: data), paths.pendingDiskUpdate)
            } catch {
                throw unreadable(paths.pendingDiskUpdate, error)
            }
        case (nil, let data?):
            // Runtime 0.1.0 staged only restores, always at one scratch name.
            struct Legacy: Codable {
                var inode: UInt64
                var record: SandboxRecord
            }
            do {
                let old = try JSONDecoder.iso.decode(Legacy.self, from: data)
                let pending = Pending(work: ".restore-rootfs.ext4", inode: old.inode, record: old.record)
                return (pending, paths.legacyPendingRestore)
            } catch {
                throw unreadable(paths.legacyPendingRestore, error)
            }
        case (_?, _?):
            throw SandboxError("\(paths.id) has two staged disk updates; compare rootfs.ext4 with record.json by hand")
        }
    }

    /// Whether `pending`'s disk is the installed one.
    static func isPublished(_ pending: Pending, _ paths: SandboxPaths) throws -> Bool {
        guard pending.record.id == paths.id else {
            throw SandboxError("\(paths.id) has a staged disk update for \(pending.record.id)")
        }
        guard let installed = SandboxPaths.inode(paths.rootfs) else {
            throw SandboxError("\(paths.id) has a staged disk update but no rootfs.ext4")
        }
        if installed == pending.inode { return true }
        let work = paths.dir.appendingPathComponent(pending.work)
        if let prepared = SandboxPaths.inode(work), prepared != pending.inode {
            throw SandboxError("\(paths.id): scratch disk \(pending.work) is not the one its staged update names")
        }
        return false
    }

    private static func read(_ url: URL) throws -> Data? {
        do {
            return try Data(contentsOf: url)
        } catch CocoaError.fileReadNoSuchFile {
            return nil
        }
    }
}
