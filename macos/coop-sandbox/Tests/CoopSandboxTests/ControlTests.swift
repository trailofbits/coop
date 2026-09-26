import Foundation
import Testing

@testable import CoopSandboxCore

@Suite struct ControlTests {
    func socketPath() -> URL {
        SandboxPaths.controlDirectory.appendingPathComponent("test-\(UUID().uuidString.prefix(8)).sock")
    }

    @Test func roundTripsRequestAndResponse() throws {
        let path = socketPath()
        defer { unlink(path.path) }
        try ControlSocket.serve(at: path) { req in
            switch req.op {
            case .exec: ControlResponse(ok: true, exitCode: 7, stdout: Data((req.argv ?? []).joined(separator: " ").utf8))
            default: ControlResponse(ok: true)
            }
        }
        #expect(try ControlSocket.call(path, .init(op: .ping)).ok)
        let r = try ControlSocket.call(path, .init(op: .exec, argv: ["echo", "hi"]))
        #expect(r.exitCode == 7)
        #expect(r.stdout == Data("echo hi".utf8))
    }

    @Test func socketIsOwnerOnly() throws {
        let path = socketPath()
        defer { unlink(path.path) }
        try ControlSocket.serve(at: path) { _ in ControlResponse(ok: true) }
        var st = stat()
        #expect(stat(path.path, &st) == 0)
        #expect(st.st_mode & 0o077 == 0)
        #expect(stat(path.deletingLastPathComponent().path, &st) == 0)
        #expect(st.st_mode & 0o077 == 0)
    }

    @Test func missingOwnerIsAnError() {
        #expect(throws: SandboxError.self) { try ControlSocket.call(socketPath(), .init(op: .ping)) }
    }

    @Test func silentOwnerTimesOut() throws {
        let path = socketPath()
        defer { unlink(path.path) }
        try ControlSocket.serve(at: path) { _ in
            try? await Task.sleep(for: .seconds(10))
            return ControlResponse(ok: true)
        }
        let start = Date()
        #expect(throws: SandboxError.self) { try ControlSocket.call(path, .init(op: .ping), timeout: 1) }
        #expect(Date().timeIntervalSince(start) < 5)
    }

    @Test func malformedRequestGetsAnErrorReply() throws {
        let path = socketPath()
        defer { unlink(path.path) }
        try ControlSocket.serve(at: path) { _ in ControlResponse(ok: true) }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
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
        try handle.write(contentsOf: Data("not json\n".utf8))
        let reply = try handle.readToEnd() ?? Data()
        let decoded = try JSONDecoder().decode(ControlResponse.self, from: reply.prefix { $0 != 0x0A })
        #expect(decoded.ok == false)
    }
}

@Suite struct ReconcileTests {
    @Test func finishesDeletesAndRemovesUncommittedCreates() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        let root = try SandboxRoot(dir.path)
        try root.createDirectories()
        let fm = FileManager.default
        try fm.createDirectory(at: root.sandboxes.appendingPathComponent(".deleting-a-1234"), withIntermediateDirectories: true)
        try fm.createDirectory(at: root.sandboxes.appendingPathComponent("half"), withIntermediateDirectories: true)
        try Data().write(to: root.bases.appendingPathComponent(".tmp-x.ext4"))
        let committed = root.sandbox(try SandboxID("kept"))
        try fm.createDirectory(at: committed.dir, withIntermediateDirectories: true)
        try committed.save(
            SandboxRecord(
                id: try SandboxID("kept"), owner: "o", imageReference: "i", imageDigest: "d", baseDisk: nil, environment: [],
                cpus: 1, memoryBytes: 1, diskBytes: 1, subnetIndex: 1, createdAt: Date()))

        let actions = try Sandboxes.reconcile(root: root)
        #expect(actions.contains { $0.action == "finished-delete" })
        #expect(!fm.fileExists(atPath: root.sandboxes.appendingPathComponent(".deleting-a-1234").path))
        #expect(actions.contains { $0.id == "half" && $0.action == "removed-uncommitted-create" })
        #expect(actions.contains { $0.id == "kept" && $0.status == "stopped" && $0.action == "none" })
        #expect(!fm.fileExists(atPath: root.sandboxes.appendingPathComponent("half").path))
        #expect(!fm.fileExists(atPath: root.bases.appendingPathComponent(".tmp-x.ext4").path))
        #expect(fm.fileExists(atPath: committed.record.path))
    }
}

@Suite struct LogTailTests {
    @Test func tailSplitsCRLFConsoleOutput() {
        let data = Data("a\r\nb\r\nc\r\n".utf8)
        #expect(String(decoding: Sandboxes.tailLines(data, 2), as: UTF8.self) == "b\r\nc\r\n")
    }

    @Test func tailOfShortOrEmptyInput() {
        #expect(Sandboxes.tailLines(Data(), 3).isEmpty)
        #expect(String(decoding: Sandboxes.tailLines(Data("only".utf8), 3), as: UTF8.self) == "only\n")
    }
}
