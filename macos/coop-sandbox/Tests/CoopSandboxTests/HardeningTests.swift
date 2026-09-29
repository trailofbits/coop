import Foundation
import Testing

@testable import CoopSandboxCore

/// Fail-closed handling of state the runtime cannot trust.
@Suite struct HardeningTests {
    func root() throws -> SandboxRoot {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        let r = try SandboxRoot(dir.path)
        try r.createDirectories()
        return r
    }

    func mode(_ url: URL) -> mode_t {
        var st = stat()
        return lstat(url.path, &st) == 0 ? st.st_mode & 0o7777 : 0
    }

    // MARK: state directories

    @Test func createDirectoriesNarrowsExistingDirectories() throws {
        let r = try root()
        for dir in [r.root, r.disks] { chmod(dir.path, 0o755) }
        try r.createDirectories()
        #expect(mode(r.root) == 0o700)
        #expect(mode(r.disks) == 0o700)
    }

    @Test func createDirectoriesRefusesASymlinkedStateDirectory() throws {
        let r = try root()
        let elsewhere = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: elsewhere, withIntermediateDirectories: true)
        try FileManager.default.removeItem(at: r.disks)
        try FileManager.default.createSymbolicLink(at: r.disks, withDestinationURL: elsewhere)
        #expect(throws: SandboxError.self) { try r.createDirectories() }
    }

    @Test func clonesAreOwnerOnly() throws {
        let r = try root()
        let source = r.root.appendingPathComponent("source.ext4")
        try Data("disk".utf8).write(to: source)
        chmod(source.path, 0o644)
        let copy = r.root.appendingPathComponent("copy.ext4")
        try clone(source, to: copy)
        #expect(mode(copy) == 0o600)
    }

    // MARK: committed disks

    func committedDisk(_ r: SandboxRoot, _ name: SandboxID, content: String) throws {
        try Data(content.utf8).write(to: r.disk(name))
        try JSONEncoder.pretty.encode(metadata(content)).write(to: Sandboxes.metadataURL(root: r, name: name))
    }

    func metadata(_ image: String) throws -> DiskMetadata {
        DiskMetadata(
            imageReference: image, imageDigest: "d", environment: [], diskBytes: 1, committedFrom: try SandboxID("src"),
            createdAt: Date())
    }

    func contents(_ url: URL) throws -> String { String(decoding: try Data(contentsOf: url), as: UTF8.self) }

    /// Interrupt a replacing commit at each boundary; readers, settles, and
    /// reconcile always pair the old disk with its metadata or the new disk
    /// with its own, and never lose the old disk.
    @Test(arguments: DiskCommit.Fault.allCases, [true, false])
    func interruptedCommitRecoversToOneSide(_ fault: DiskCommit.Fault, _ replacing: Bool) async throws {
        let r = try root()
        let name = try SandboxID("snap")
        if replacing { try committedDisk(r, name, content: "old") }
        let work = r.disks.appendingPathComponent(".tmp-snap-1234.ext4")
        try Data("new".utf8).write(to: work)

        #expect(throws: DiskCommit.InjectedFault.self) {
            try DiskCommit.publish(r, name, work: work, metadata: try metadata("new"), fault: fault)
        }
        let applied = fault != .afterStaging
        func consistent() async throws {
            if !applied && !replacing {
                #expect(!FileManager.default.fileExists(atPath: r.disk(name).path), "\(fault)")
                return
            }
            let want = applied ? "new" : "old"
            #expect(try contents(r.disk(name)) == want, "\(fault) \(replacing)")
            let meta = try await Sandboxes.withDisk(r, name, .shared) { try Sandboxes.loadDiskMetadata(root: r, name: name) }
            #expect(meta.imageReference == want, "\(fault) \(replacing)")
        }
        try await consistent()
        _ = try Sandboxes.reconcile(root: r)
        try await consistent()
        #expect(!FileManager.default.fileExists(atPath: DiskCommit.pendingURL(r, name).path))
        #expect(!FileManager.default.fileExists(atPath: work.path))
        #expect(try DiskCommit.settle(r, name) == false)
        try await consistent()
    }

    @Test func unreadableStagedCommitFailsClosed() async throws {
        let r = try root()
        let name = try SandboxID("snap")
        try committedDisk(r, name, content: "old")
        try Data("{".utf8).write(to: DiskCommit.pendingURL(r, name))
        await #expect(throws: SandboxError.self) {
            try await Sandboxes.withDisk(r, name, .shared) { try Sandboxes.loadDiskMetadata(root: r, name: name) }
        }
        let actions = try Sandboxes.reconcile(root: r)
        #expect(actions.contains { $0.id == "snap" && $0.action.hasPrefix("unresolved-disk-commit") })
        #expect(FileManager.default.fileExists(atPath: r.disk(name).path))
    }

    // MARK: owner state

    @Test func unprobeableOwnerLockCountsAsHeld() throws {
        let r = try root()
        let paths = r.sandbox(try SandboxID("a"))
        try FileManager.default.createDirectory(at: paths.dir, withIntermediateDirectories: true)
        #expect(!Sandboxes.ownerHoldsLock(paths))
        try Data().write(to: paths.lock)
        chmod(paths.lock.path, 0)
        #expect(Sandboxes.ownerHoldsLock(paths))
        #expect(Sandboxes.status(paths) != .stopped)
        let why = try #require(Sandboxes.lockProbeFailure(paths))
        #expect(why.contains("Permission denied"))
        #expect(throws: SandboxError.self) { try Sandboxes.requireStopped(paths, SandboxID("a")) }
    }

    @Test func previousOwnerFailureIsClearedOrReported() throws {
        let r = try root()
        let paths = r.sandbox(try SandboxID("a"))
        try FileManager.default.createDirectory(at: paths.dir, withIntermediateDirectories: true)
        try Sandboxes.clearOwnerFailure(paths)
        try Data("old failure".utf8).write(to: paths.ownerFailed)
        try Sandboxes.clearOwnerFailure(paths)
        #expect(!FileManager.default.fileExists(atPath: paths.ownerFailed.path))
        try FileManager.default.createDirectory(at: paths.ownerFailed, withIntermediateDirectories: true)
        #expect(throws: SandboxError.self) { try Sandboxes.clearOwnerFailure(paths) }
    }

    // MARK: subnets

    @Test func corruptSubnetStateFailsClosed() throws {
        let r = try root()
        try Data("{".utf8).write(to: r.subnetState)
        #expect(throws: SandboxError.self) { try SubnetAllocator(root: r).allocate(for: try SandboxID("a")) { _ in } }
        #expect(try contents(r.subnetState) == "{")
    }

    // MARK: control requests

    @Test func readLineStopsAtTheLimit() throws {
        func read(_ bytes: String, limit: Int?) throws -> ControlSocket.ReadResult {
            let pipe = Pipe()
            try pipe.fileHandleForWriting.write(contentsOf: Data(bytes.utf8))
            try pipe.fileHandleForWriting.close()
            return ControlSocket.readLine(pipe.fileHandleForReading, limit: limit)
        }
        #expect(try read("abc\nrest", limit: 3) == .line(Data("abc".utf8)))
        #expect(try read("abcd\n", limit: 3) == .tooLong)
        #expect(try read("abcd", limit: 3) == .tooLong)
        #expect(try read("abcd\n", limit: nil) == .line(Data("abcd".utf8)))
        #expect(try read("", limit: 3) == .closed)
    }

    @Test func oversizedRequestIsRejected() throws {
        let path = SandboxPaths.controlDirectory.appendingPathComponent("test-\(UUID().uuidString.prefix(8)).sock")
        defer { unlink(path.path) }
        try ControlSocket.serve(at: path) { _ in ControlResponse(ok: true) }
        let stdin = Data(count: ControlSocket.maxRequestBytes)
        #expect(throws: SandboxError.self) { try ControlSocket.call(path, .init(op: .exec, argv: ["cat"], stdin: stdin)) }

        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            let b = Array(path.path.utf8)
            raw.copyBytes(from: b)
            raw[b.count] = 0
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        #expect(rc == 0)
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        // No newline ever arrives; the owner must stop reading at its limit.
        Thread.detachNewThread {
            let chunk = [UInt8](repeating: 0x61, count: 65536)
            var sent = 0
            while sent <= ControlSocket.maxRequestBytes + chunk.count {
                let n = write(fd, chunk, chunk.count)
                if n <= 0 { break }
                sent += n
            }
        }
        let reply = try handle.readToEnd() ?? Data()
        let decoded = try JSONDecoder().decode(ControlResponse.self, from: reply.prefix { $0 != 0x0A })
        #expect(decoded.ok == false)
        #expect(decoded.error?.contains("exceeds") == true)
    }
}
