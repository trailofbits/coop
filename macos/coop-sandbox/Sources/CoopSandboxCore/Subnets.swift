import Foundation

/// Allocates one `10.231.N.0/24` per sandbox across the whole state root.
///
/// vmnet keeps a subnet reserved after its owning process dies uncleanly and
/// refuses to recreate it, so a subnet that fails to come up is quarantined
/// for a while rather than retried at once. Quarantine is bounded in size
/// and time, so a leak can never exhaust the range permanently.
public struct SubnetAllocator: Sendable {
    public static let range = 1...250
    public static let quarantineTTL: TimeInterval = 24 * 60 * 60
    public static let maxQuarantined = 64

    struct State: Codable {
        var quarantined: [Int: Date] = [:]
    }

    let root: SandboxRoot

    public init(root: SandboxRoot) { self.root = root }

    /// Runs `body` under an exclusive lock on the allocation state.
    func locked<T>(_ body: (inout State) throws -> T) throws -> T {
        let lock = try FileLock.acquire(root.allocationLock, .exclusive)
        defer { withExtendedLifetime(lock) {} }
        var state = try loadState()
        let result = try body(&state)
        try JSONEncoder.pretty.encode(state).write(to: root.subnetState, options: .atomic)
        return result
    }

    /// A missing file is an empty quarantine. One that cannot be read is an
    /// error: resetting it would hand out subnets vmnet still refuses.
    func loadState() throws -> State {
        let data: Data
        do {
            data = try Data(contentsOf: root.subnetState)
        } catch CocoaError.fileReadNoSuchFile {
            return State()
        }
        do {
            return try JSONDecoder.iso.decode(State.self, from: data)
        } catch {
            throw SandboxError("unreadable subnet state \(root.subnetState.path) (\(error)); remove it to clear the quarantine")
        }
    }

    static func prune(_ state: inout State, now: Date) {
        state.quarantined = state.quarantined.filter { now.timeIntervalSince($0.value) < quarantineTTL }
        while state.quarantined.count > maxQuarantined, let oldest = state.quarantined.min(by: { $0.value < $1.value }) {
            state.quarantined.removeValue(forKey: oldest.key)
        }
    }

    static func pick(used: Set<Int>, state: State) -> Int? {
        range.first { !used.contains($0) && state.quarantined[$0] == nil }
    }

    /// Picks a free index (not used by another sandbox, not quarantined) and
    /// passes it to `commit`, which must persist it before the lock is
    /// released so concurrent allocations never collide.
    public func allocate(for id: SandboxID, now: Date = Date(), commit: (Int) throws -> Void) throws {
        try locked { state in
            Self.prune(&state, now: now)
            let used = Set(try root.allRecords().filter { $0.id != id }.map(\.subnetIndex))
            guard let index = Self.pick(used: used, state: state) else {
                throw SandboxError("no free sandbox subnet (\(used.count) in use, \(state.quarantined.count) quarantined)")
            }
            try commit(index)
        }
    }

    /// Quarantines `index` (vmnet refused it), then allocates a replacement
    /// the same way as ``allocate(for:now:commit:)``.
    public func quarantineAndReallocate(_ index: Int, for id: SandboxID, now: Date = Date(), commit: (Int) throws -> Void) throws {
        try locked { state in
            state.quarantined[index] = now
            Self.prune(&state, now: now)
            let used = Set(try root.allRecords().filter { $0.id != id }.map(\.subnetIndex))
            guard let next = Self.pick(used: used, state: state) else {
                throw SandboxError("no free sandbox subnet after quarantining \(index)")
            }
            try commit(next)
        }
    }
}
