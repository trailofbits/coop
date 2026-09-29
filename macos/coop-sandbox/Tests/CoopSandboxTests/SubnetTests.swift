import Foundation
import Testing

@testable import CoopSandboxCore

@Suite struct SubnetTests {
    func tempRoot() throws -> SandboxRoot {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("csb-\(UUID().uuidString)")
        let root = try SandboxRoot(dir.path)
        try root.createDirectories()
        return root
    }

    func commitRecord(_ root: SandboxRoot, _ name: String, _ index: Int) throws {
        let id = try SandboxID(name)
        let paths = root.sandbox(id)
        try FileManager.default.createDirectory(at: paths.dir, withIntermediateDirectories: true)
        try paths.save(
            SandboxRecord(
                id: id, owner: "o", imageReference: "i", imageDigest: "d", baseDisk: nil, environment: [], cpus: 1,
                memoryBytes: 1, diskBytes: 1, subnetIndex: index, createdAt: Date()))
    }

    @Test func allocatesLowestFreeIndex() throws {
        let root = try tempRoot()
        try commitRecord(root, "a", 1)
        try commitRecord(root, "b", 3)
        var got = 0
        try SubnetAllocator(root: root).allocate(for: try SandboxID("c")) { got = $0 }
        #expect(got == 2)
    }

    @Test func ownIndexIsNotConsideredUsed() throws {
        let root = try tempRoot()
        try commitRecord(root, "a", 1)
        var got = 0
        try SubnetAllocator(root: root).allocate(for: try SandboxID("a")) { got = $0 }
        #expect(got == 1)
    }

    @Test func quarantinedIndexIsSkippedUntilExpiry() throws {
        let root = try tempRoot()
        let alloc = SubnetAllocator(root: root)
        let id = try SandboxID("a")
        let t0 = Date(timeIntervalSince1970: 1_000_000)
        var got = 0
        try alloc.quarantineAndReallocate(1, for: id, now: t0) { got = $0 }
        #expect(got == 2)
        try alloc.allocate(for: id, now: t0.addingTimeInterval(60)) { got = $0 }
        #expect(got == 2)
        try alloc.allocate(for: id, now: t0.addingTimeInterval(SubnetAllocator.quarantineTTL + 1)) { got = $0 }
        #expect(got == 1)
    }

    @Test func quarantineIsBounded() {
        var state = SubnetAllocator.State()
        let now = Date()
        for i in 1...(SubnetAllocator.maxQuarantined + 20) {
            state.quarantined[i] = now.addingTimeInterval(TimeInterval(i))
        }
        SubnetAllocator.prune(&state, now: now)
        #expect(state.quarantined.count == SubnetAllocator.maxQuarantined)
        // The oldest entries are the ones dropped.
        #expect(state.quarantined[1] == nil)
    }

    @Test func failedCommitAllocatesNothing() throws {
        let root = try tempRoot()
        struct Boom: Error {}
        #expect(throws: Boom.self) { try SubnetAllocator(root: root).allocate(for: try SandboxID("a")) { _ in throw Boom() } }
        #expect(try root.allRecords().isEmpty)
    }
}
