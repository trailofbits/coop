import CryptoKit
import Foundation
import Testing

@testable import CoopSandboxCore

@Suite struct SandboxIDTests {
    @Test(arguments: ["a", "coop-1f2e3d4c-0011223344556677", "x0"])
    func acceptsPathSafeIDs(_ raw: String) throws {
        #expect(try SandboxID(raw).rawValue == raw)
    }

    @Test(arguments: ["", "-a", "../x", "a/b", "A", "a b", ".x", String(repeating: "a", count: 49)])
    func rejectsUnsafeIDs(_ raw: String) {
        #expect(throws: SandboxError.self) { try SandboxID(raw) }
    }

    @Test func decodingValidates() {
        #expect(throws: (any Error).self) { try JSONDecoder().decode(SandboxID.self, from: Data("\"../etc\"".utf8)) }
    }
}

@Suite struct RootTests {
    @Test func rootMustBeAbsolute() {
        #expect(throws: SandboxError.self) { try SandboxRoot("relative/root") }
    }

    @Test func controlSocketPathFitsSunPath() throws {
        let root = try SandboxRoot("/" + String(repeating: "deep/", count: 60))
        let path = root.sandbox(try SandboxID("coop-aaaaaaaa-0123456789abcdef")).control.path
        #expect(path.utf8.count < 104)
    }

    /// A new root takes the requested kernel; an initialized root keeps its
    /// own while it is pinned, and a pinned `--kernel` replaces one that is not.
    @Test func kernelSelectionPrefersTheInstalledKernel() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        let root = try SandboxRoot(dir.appendingPathComponent("runtime").path)
        try root.createDirectories()
        let pinned = dir.appendingPathComponent("pinned"), other = dir.appendingPathComponent("other")
        try Data("pinned".utf8).write(to: pinned)
        try Data("other".utf8).write(to: other)
        let sha = SHA256.hash(data: Data("pinned".utf8)).map { String(format: "%02x", $0) }.joined()

        #expect(throws: SandboxError.self) { try KernelPin.select(root: root, requested: other.path, allowed: [sha]) }
        let fresh = try KernelPin.select(root: root, requested: pinned.path, allowed: [sha])
        #expect(fresh.install && fresh.sha256 == sha)
        try fresh.data.write(to: root.kernel)

        let again = try KernelPin.select(root: root, requested: other.path, allowed: [sha])
        #expect(!again.install && again.sha256 == sha)
        #expect(throws: SandboxError.self) { try KernelPin.select(root: root, requested: pinned.path, allowed: []) }

        try Data("other".utf8).write(to: root.kernel)
        let replaced = try KernelPin.select(root: root, requested: pinned.path, allowed: [sha])
        #expect(replaced.install && replaced.data == Data("pinned".utf8))
    }

    @Test func labelsDifferPerRoot() throws {
        let id = try SandboxID("s")
        #expect(try SandboxRoot("/a").sandbox(id).launchdLabel != SandboxRoot("/b").sandbox(id).launchdLabel)
    }

    @Test func recordRoundTripsAndDerivesSubnet() throws {
        let r = SandboxRecord(
            id: try SandboxID("a"), owner: "o", imageReference: "i", imageDigest: "d", baseDisk: nil, environment: ["A=1"],
            cpus: 2, memoryBytes: 1, diskBytes: 2, subnetIndex: 7, createdAt: Date(timeIntervalSince1970: 0))
        let back = try JSONDecoder.iso.decode(SandboxRecord.self, from: JSONEncoder.pretty.encode(r))
        #expect(back.subnet == "10.231.7.0/24")
        #expect(back.environment == ["A=1"])
    }

    @Test func recordHasNoHostExposureFields() throws {
        let r = SandboxRecord(
            id: try SandboxID("a"), owner: "o", imageReference: "i", imageDigest: "d", baseDisk: nil, environment: [],
            cpus: 1, memoryBytes: 1, diskBytes: 1, subnetIndex: 1, createdAt: Date())
        let keys = Set(((try JSONSerialization.jsonObject(with: JSONEncoder.pretty.encode(r)) as? [String: Any]) ?? [:]).keys)
        for forbidden in ["mounts", "sockets", "publishedPorts", "ssh", "sshAgent", "volumes"] {
            #expect(!keys.contains(forbidden))
        }
    }
}

@Suite struct CanonicalRootTests {
    @Test func nonexistentRootResolvesThroughItsParent() throws {
        // /tmp is a symlink to /private/tmp on macOS.
        let name = "csb-\(UUID().uuidString)"
        let before = try SandboxRoot("/tmp/\(name)/runtime").root.path
        #expect(before == "/private/tmp/\(name)/runtime")
        try FileManager.default.createDirectory(atPath: before, withIntermediateDirectories: true)
        #expect(try SandboxRoot("/tmp/\(name)/runtime").root.path == before)
    }
}
