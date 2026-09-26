import Foundation
import Testing

@testable import CoopSandboxCore

/// Guards that run before any VM work, so they are testable without one.
@Suite struct GuardTests {
    func root() throws -> SandboxRoot {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        let r = try SandboxRoot(dir.path)
        try r.createDirectories()
        return r
    }

    @discardableResult
    func stoppedSandbox(_ root: SandboxRoot, _ name: String, owner: String = "o", disk: UInt64 = 8 << 30) throws -> SandboxPaths {
        let id = try SandboxID(name)
        let paths = root.sandbox(id)
        try FileManager.default.createDirectory(at: paths.dir, withIntermediateDirectories: true)
        try paths.save(
            SandboxRecord(
                id: id, owner: owner, imageReference: "i", imageDigest: "d", baseDisk: nil, environment: [], cpus: 1,
                memoryBytes: 1 << 30, diskBytes: disk, subnetIndex: 1, createdAt: Date()))
        try Data().write(to: paths.rootfs)
        return paths
    }

    @Test func deleteRefusesAnotherOwner() async throws {
        let r = try root()
        let paths = try stoppedSandbox(r, "a", owner: "mine")
        await #expect(throws: SandboxError.self) { try await Sandboxes.delete(root: r, id: try SandboxID("a"), owner: "theirs") }
        #expect(FileManager.default.fileExists(atPath: paths.record.path))
        try await Sandboxes.delete(root: r, id: try SandboxID("a"), owner: "mine")
        #expect(!FileManager.default.fileExists(atPath: paths.dir.path))
    }

    @Test func growRefusesToShrinkOrKeepSize() async throws {
        let r = try root()
        try stoppedSandbox(r, "a", disk: 8 << 30)
        for bytes: UInt64 in [4 << 30, 8 << 30] {
            await #expect(throws: SandboxError.self) { try await Sandboxes.grow(root: r, id: try SandboxID("a"), diskBytes: bytes) }
        }
    }

    @Test func commitRefusesAnExistingNameWithoutReplace() async throws {
        let r = try root()
        try stoppedSandbox(r, "a")
        try Data().write(to: r.disk(try SandboxID("taken")))
        await #expect(throws: SandboxError.self) {
            try await Sandboxes.commit(root: r, id: try SandboxID("a"), name: try SandboxID("taken"), replace: false)
        }
    }

    @Test func restoreFromAMissingDiskChangesNothing() async throws {
        let r = try root()
        let paths = try stoppedSandbox(r, "a")
        let before = try paths.loadRecord()
        await #expect(throws: SandboxError.self) {
            try await Sandboxes.restore(root: r, id: try SandboxID("a"), source: .disk(try SandboxID("missing")))
        }
        #expect(try paths.loadRecord().diskGeneration == before.diskGeneration)
    }

    /// A restore staged by runtime 0.1.0 and interrupted after its disk swap
    /// still commits; one interrupted before the swap is discarded.
    @Test func legacyStagedRestoreIsRecovered() async throws {
        struct Legacy: Codable {
            var inode: UInt64
            var record: SandboxRecord
        }
        let r = try root()
        let paths = try stoppedSandbox(r, "a")
        let legacyWork = paths.dir.appendingPathComponent(".restore-rootfs.ext4")
        let legacyPending = paths.dir.appendingPathComponent("restore.pending.json")
        var next = try paths.loadRecord()
        next.diskGeneration += 1
        next.imageReference = "restored"

        try Data("new".utf8).write(to: legacyWork)
        try JSONEncoder.pretty.encode(Legacy(inode: SandboxPaths.inode(legacyWork)!, record: next)).write(to: legacyPending)
        #expect(rename(legacyWork.path, paths.rootfs.path) == 0)
        #expect(try paths.loadRecord().imageReference == "restored", "readers see the committed update")
        #expect(try paths.readRecordFile().diskGeneration == 0, "but only a guarded mutation writes it")
        #expect(try DiskUpdate.settle(paths))
        #expect(try paths.readRecordFile().imageReference == "restored")
        #expect(!FileManager.default.fileExists(atPath: legacyPending.path))

        var later = next
        later.diskGeneration += 1
        try Data("newer".utf8).write(to: legacyWork)
        try JSONEncoder.pretty.encode(Legacy(inode: SandboxPaths.inode(legacyWork)!, record: later)).write(to: legacyPending)
        #expect(try paths.loadRecord().diskGeneration == next.diskGeneration)
        _ = try await Sandboxes.setResources(root: r, id: try SandboxID("a"), cpus: 2, memoryBytes: nil)
        #expect(try paths.loadRecord().diskGeneration == next.diskGeneration)
        #expect(!FileManager.default.fileExists(atPath: legacyPending.path))
        #expect(!FileManager.default.fileExists(atPath: legacyWork.path))
        #expect(try Data(contentsOf: paths.rootfs) == Data("new".utf8))
    }

    @Test func mutationsRequireAStoppedSandbox() async throws {
        let r = try root()
        let paths = try stoppedSandbox(r, "a")
        try JSONEncoder.pretty.encode(LiveState(pid: getpid(), startedAt: Date(), ipv4: nil, ipv6: nil)).write(to: paths.live)
        #expect(Sandboxes.status(paths) != .stopped)
        await #expect(throws: SandboxError.self) {
            try await Sandboxes.setResources(root: r, id: try SandboxID("a"), cpus: 2, memoryBytes: nil)
        }
    }

    @Test func unreadableRecordFailsListingsClosed() throws {
        let r = try root()
        try stoppedSandbox(r, "a")
        try Data("{".utf8).write(to: r.sandbox(try SandboxID("a")).record)
        #expect(throws: (any Error).self) { try r.allRecords() }
    }

    @Test func reconcileRemovesScratchAndDisksWithoutMetadata() throws {
        let r = try root()
        let paths = try stoppedSandbox(r, "a")
        for name in [".update-x.ext4", ".grow-rootfs.ext4", ".restore-rootfs.ext4", ".maintenance-x.ext4"] {
            try Data().write(to: paths.dir.appendingPathComponent(name))
        }
        try Data().write(to: r.disk(try SandboxID("orphan")))
        try Data().write(to: r.disk(try SandboxID("kept")))
        try Data("{}".utf8).write(to: r.disks.appendingPathComponent("kept.json"))
        let actions = try Sandboxes.reconcile(root: r)
        let left = try FileManager.default.contentsOfDirectory(atPath: paths.dir.path)
        #expect(!left.contains { Sandboxes.isScratch($0) })
        #expect(left.contains("rootfs.ext4"))
        #expect(!FileManager.default.fileExists(atPath: r.disk(try SandboxID("orphan")).path))
        #expect(FileManager.default.fileExists(atPath: r.disk(try SandboxID("kept")).path))
        #expect(actions.contains { $0.action == "removed-uncommitted-disk" })
    }

    @Test func launchdJobRestartsOnlyAbnormalExitsWithAFixedEnvironment() throws {
        let plist = Launchd.plist(label: "l", executable: "/x", arguments: ["run"], log: URL(fileURLWithPath: "/tmp/l"))
        #expect(plist["KeepAlive"] as? [String: Bool] == ["SuccessfulExit": false])
        let env = plist["EnvironmentVariables"] as? [String: String] ?? [:]
        #expect(Set(env.keys) == ["PATH", "HOME"])
        #expect(plist["ProgramArguments"] as? [String] == ["/x", "run"])
    }

    @Test func guestOutputIsCapped() throws {
        let w = BufferWriter()
        try w.write(Data(count: BufferWriter.limit - 1))
        try w.write(Data(count: 10))
        try w.write(Data(count: 10))
        #expect(w.data.count == BufferWriter.limit)
    }

    /// Past the cap the console file restarts with a marker and keeps the
    /// newest bytes, so a guest flooding its console cannot fill the host disk.
    @Test func consoleLogIsCappedAndKeepsTheNewestBytes() throws {
        let r = try root()
        let url = r.root.appendingPathComponent("boot.log")
        let old = Data(repeating: 0x4F, count: 700)
        try old.write(to: url)
        let out = try FileHandle(forWritingTo: url)
        try out.seekToEnd()
        let pipe = Pipe()
        for byte: UInt8 in [0x61, 0x62, 0x63] { pipe.fileHandleForWriting.write(Data(repeating: byte, count: 400)) }
        try pipe.fileHandleForWriting.close()
        ConsoleLog.copy(from: pipe.fileHandleForReading, to: out, written: old.count, limit: 1000)
        let log = try Data(contentsOf: url)
        #expect(log.count <= 1000)
        #expect(!log.contains(0x4F), "the earlier log is truncated, not overwritten in place")
        #expect(String(decoding: log, as: UTF8.self).hasPrefix("[coop-sandbox: console log passed 1000 bytes"))
        #expect(log.suffix(400) == Data(repeating: 0x63, count: 400))
    }

    /// A restart shorter than the old log leaves none of it behind.
    @Test func consoleLogRestartTruncatesTheOldLog() throws {
        let r = try root()
        let url = r.root.appendingPathComponent("boot.log")
        try Data(repeating: 0x4F, count: 900).write(to: url)
        let out = try FileHandle(forWritingTo: url)
        try out.seekToEnd()
        let pipe = Pipe()
        pipe.fileHandleForWriting.write(Data(repeating: 0x63, count: 200))
        try pipe.fileHandleForWriting.close()
        ConsoleLog.copy(from: pipe.fileHandleForReading, to: out, written: 900, limit: 1000)
        let log = try Data(contentsOf: url)
        #expect(!log.contains(0x4F))
        #expect(log.suffix(200) == Data(repeating: 0x63, count: 200))
    }

    /// A host write error (here, a read-only handle) stops logging but not
    /// the drain: the owner and its VM keep running and the console never blocks.
    @Test func consoleWriteErrorsAreSurvivedAndTheConsoleDrained() throws {
        let r = try root()
        let url = r.root.appendingPathComponent("boot.log")
        try Data().write(to: url)
        let pipe = Pipe()
        let readOnly = try FileHandle(forReadingFrom: url)
        let drained = Thread { ConsoleLog.copy(from: pipe.fileHandleForReading, to: readOnly, written: 0, limit: 1 << 20) }
        drained.start()
        // More than a pipe buffer: this only completes if the drain keeps reading.
        for _ in 0..<4 { try pipe.fileHandleForWriting.write(contentsOf: Data(repeating: 0x61, count: 32 * 1024)) }
        try pipe.fileHandleForWriting.close()
        #expect(try Data(contentsOf: url).isEmpty)
    }

    /// A console log already past the cap from earlier boots starts over.
    @Test func consoleLogOverTheCapStartsOverAtBoot() throws {
        let r = try root()
        let url = r.root.appendingPathComponent("boot.log")
        try Data(count: ConsoleLog.limit + 1).write(to: url)
        let console = try ConsoleLog(url: url)
        #expect(try FileManager.default.attributesOfItem(atPath: url.path)[.size] as? Int == 0)
        try console.writer.close()
    }

    @Test func logTailReadsOnlyTheEndOfTheFile() throws {
        let r = try root()
        let url = r.root.appendingPathComponent("boot.log")
        try (Data(repeating: 0x78, count: 4096) + Data("\nlast\n".utf8)).write(to: url)
        #expect(Sandboxes.tailData(url, lines: 5, window: 16) == Data("last\n".utf8))
        #expect(Sandboxes.tailData(url, lines: 1) == Data("last\n".utf8))
    }

    /// Holds `paths`' owner lock on a separate open file, as a live owner does.
    func holdOwnerLock(_ paths: SandboxPaths) -> Int32 {
        let fd = open(paths.lock.path, O_RDWR | O_CREAT | O_CLOEXEC, 0o600)
        #expect(fd >= 0)
        #expect(flock(fd, LOCK_EX) == 0)
        return fd
    }

    @Test func aHeldOwnerLockIsAliveNotCrashed() throws {
        let r = try root()
        let paths = try stoppedSandbox(r, "a")
        let fd = holdOwnerLock(paths)
        #expect(Sandboxes.status(paths) == .booting, "owner launched, live.json not yet written")
        try JSONEncoder.pretty.encode(LiveState(pid: 999_999, startedAt: Date(), ipv4: nil, ipv6: nil)).write(to: paths.live)
        #expect(Sandboxes.status(paths) == .booting, "a live owner whose control socket is not up yet")
        close(fd)
        #expect(Sandboxes.status(paths) == .crashed)
    }

    @Test func reconcileKeepsAnUnrecordedSandboxWhoseOwnerIsAlive() throws {
        let r = try root()
        let alive = r.sandbox(try SandboxID("alive"))
        let dead = r.sandbox(try SandboxID("dead"))
        for p in [alive, dead] { try FileManager.default.createDirectory(at: p.dir, withIntermediateDirectories: true) }
        let fd = holdOwnerLock(alive)
        defer { close(fd) }
        _ = try Sandboxes.reconcile(root: r)
        #expect(FileManager.default.fileExists(atPath: alive.dir.path))
        #expect(!FileManager.default.fileExists(atPath: dead.dir.path))
    }

    /// A live.json whose owner no longer holds the lock is a crash, even if
    /// its PID now belongs to another process (here, this test).
    @Test func liveStateWithoutTheOwnerLockIsCrashed() throws {
        let r = try root()
        let paths = try stoppedSandbox(r, "a")
        let live = LiveState(pid: getpid(), startedAt: Date(), ipv4: nil, ipv6: nil)
        try JSONEncoder.pretty.encode(live).write(to: paths.live)
        #expect(paths.loadLive() != nil)
        #expect(Sandboxes.status(paths) == .crashed)
    }

    @Test func controlDirectoryIsInThePrivateUserTempDir() throws {
        let parent = SandboxPaths.controlDirectory.deletingLastPathComponent()
        var st = stat()
        #expect(lstat(parent.path, &st) == 0)
        #expect(st.st_uid == getuid())
        #expect(st.st_mode & 0o077 == 0)
        #expect(!SandboxPaths.controlDirectory.path.hasPrefix("/tmp/"))
    }

    /// A working socket in a directory others can write is refused before
    /// connecting: someone else could have put it there.
    @Test func clientRefusesAnOpenControlDirectory() throws {
        let dir = URL(fileURLWithPath: "/tmp/csb-open-\(UUID().uuidString.prefix(8))")
        defer { try? FileManager.default.removeItem(at: dir) }
        let sock = dir.appendingPathComponent("s.sock")
        try ControlSocket.serve(at: sock) { _ in ControlResponse(ok: true) }
        #expect(try ControlSocket.call(sock, .init(op: .ping)).ok)
        chmod(dir.path, 0o777)
        do {
            _ = try ControlSocket.call(sock, .init(op: .ping))
            Issue.record("an open control directory was accepted")
        } catch let error as SandboxError {
            #expect(error.description.contains("control directory"), "\(error)")
        }
    }
}

@Suite struct OperationLockTests {
    @Test func reconcileLeavesScratchAloneWhileAnOperationRuns() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        let r = try SandboxRoot(dir.path)
        try r.createDirectories()
        let scratch = r.bases.appendingPathComponent(".tmp-inflight.ext4")
        try Data().write(to: scratch)
        let half = r.sandboxes.appendingPathComponent("creating")
        try FileManager.default.createDirectory(at: half, withIntermediateDirectories: true)
        do {
            let op = try OperationLock.shared(r)
            let actions = try Sandboxes.reconcile(root: r)
            #expect(actions.contains { $0.action == "sweep-skipped-operation-in-progress" })
            #expect(FileManager.default.fileExists(atPath: scratch.path))
            #expect(FileManager.default.fileExists(atPath: half.path))
            withExtendedLifetime(op) {}
        }
        _ = try Sandboxes.reconcile(root: r)
        #expect(!FileManager.default.fileExists(atPath: scratch.path))
        #expect(!FileManager.default.fileExists(atPath: half.path))
    }
}

@Suite struct IdentityResetScriptTests {
    /// Runs the reset script with `/coopdisk` rewritten to a temp tree.
    func run(_ disk: URL) throws -> Int32 {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = ["-c", Maintenance.resetIdentityScript.replacingOccurrences(of: "/coopdisk", with: disk.path)]
        p.standardError = FileHandle.nullDevice
        try p.run()
        p.waitUntilExit()
        return p.terminationStatus
    }

    func disk() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("disk-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: d.appendingPathComponent("etc/ssh"), withIntermediateDirectories: true)
        return d
    }

    @Test func removesHostKeysAndEmptiesMachineID() throws {
        let d = try disk()
        for f in ["etc/ssh/ssh_host_ed25519_key", "etc/ssh/ssh_host_ed25519_key.pub", "etc/ssh/sshd_config"] {
            try Data("x".utf8).write(to: d.appendingPathComponent(f))
        }
        try Data("abc\n".utf8).write(to: d.appendingPathComponent("etc/machine-id"))
        #expect(try run(d) == 0)
        let left = try FileManager.default.contentsOfDirectory(atPath: d.appendingPathComponent("etc/ssh").path)
        #expect(left == ["sshd_config"])
        #expect(try Data(contentsOf: d.appendingPathComponent("etc/machine-id")).isEmpty)
    }

    @Test func refusesASymlinkedSSHDirectory() throws {
        let d = try disk()
        let elsewhere = try disk()
        let victim = elsewhere.appendingPathComponent("etc/ssh/ssh_host_ed25519_key")
        try Data("keep".utf8).write(to: victim)
        try FileManager.default.removeItem(at: d.appendingPathComponent("etc/ssh"))
        try FileManager.default.createSymbolicLink(at: d.appendingPathComponent("etc/ssh"), withDestinationURL: elsewhere.appendingPathComponent("etc/ssh"))
        #expect(try run(d) == 3)
        #expect(FileManager.default.fileExists(atPath: victim.path))
    }

    @Test func aSymlinkedMachineIDIsReplacedNotFollowed() throws {
        let d = try disk()
        let target = FileManager.default.temporaryDirectory.appendingPathComponent("mid-\(UUID().uuidString)")
        try Data("host".utf8).write(to: target)
        try FileManager.default.createSymbolicLink(at: d.appendingPathComponent("etc/machine-id"), withDestinationURL: target)
        #expect(try run(d) == 0)
        #expect(try Data(contentsOf: target) == Data("host".utf8))
        let attrs = try FileManager.default.attributesOfItem(atPath: d.appendingPathComponent("etc/machine-id").path)
        #expect(attrs[.type] as? FileAttributeType == .typeRegular)
    }
}
