import Containerization
import ContainerizationEXT4
import ContainerizationOCI
import Foundation
import SystemPackage

/// The disk maintenance VMs boot from: a small image built for the purpose
/// (a shell and e2fsprogs), unpacked by `maintenance install` into the
/// root's `maintenance/` directory. It is kept apart from the image store,
/// so deleting or replacing an application image never affects it, and its
/// capacity follows its own content, never an application image's.
public struct MaintenanceArtifact: Codable, Sendable {
    /// Caller-chosen version of the image's recipe; a caller reinstalls
    /// when it wants another.
    public var version: String
    public var reference: String
    /// Content digest of the image it was unpacked from.
    public var digest: String
    public var capacityBytes: UInt64
    public var installedAt: Date
    /// Basename of the unpacked disk in `maintenance/`.
    public var disk: String
}

/// Offline disk maintenance in a short-lived VM.
///
/// The VM boots from a throwaway APFS clone of the installed
/// ``MaintenanceArtifact``, with no network, and the target disk attached
/// either as a raw device or mounted at `/coopdisk`. The target is data
/// only: none of its programs run.
public enum Maintenance {
    enum Attach {
        /// The device node itself, for fsck/resize of an unmounted filesystem.
        case raw
        /// The filesystem, mounted read-write.
        case mounted
    }

    /// Programs the maintenance scripts run; `install` refuses an image
    /// without them.
    static let requiredPrograms = ["/bin/sh", "/bin/rm", "/bin/sync", "/sbin/e2fsck", "/sbin/resize2fs"]
    /// Caps the image's compressed layers, and so the capacity
    /// ``capacity(packedBytes:)`` gives its disk.
    static let maxPackedBytes: UInt64 = 512 * 1024 * 1024

    static func artifactURL(_ root: SandboxRoot) -> URL { root.maintenance.appendingPathComponent("tools.json") }
    static func lockURL(_ root: SandboxRoot) -> URL { root.locks.appendingPathComponent("maintenance.lock") }

    /// Filesystem capacity for an image whose layers total `packed` bytes
    /// compressed: room for 4x expansion plus 512 MiB, in 64 MiB steps. The
    /// file is sparse, so unused capacity costs nothing.
    static func capacity(packedBytes packed: UInt64) throws -> UInt64 {
        guard packed <= maxPackedBytes else {
            throw SandboxError("maintenance image layers total \(packed) bytes; a maintenance image must stay under \(maxPackedBytes)")
        }
        let step: UInt64 = 64 * 1024 * 1024
        let wanted = packed * 4 + 512 * 1024 * 1024
        return (wanted + step - 1) / step * step
    }

    /// Total of `sizes` (layer sizes from a manifest), saturating just past
    /// ``maxPackedBytes`` so an absurd manifest is refused, not overflowed.
    static func packedBytes(_ sizes: [Int64]) -> UInt64 {
        sizes.reduce(UInt64(0)) { sum, size in
            min(sum + min(UInt64(max(0, size)), maxPackedBytes + 1), maxPackedBytes + 1)
        }
    }

    public static func installed(root: SandboxRoot) throws -> MaintenanceArtifact? {
        let url = artifactURL(root)
        guard FileManager.default.fileExists(atPath: url.path) else { return nil }
        return try JSONDecoder.iso.decode(MaintenanceArtifact.self, from: Data(contentsOf: url))
    }

    /// Unpack `reference` from the image store as the maintenance boot disk
    /// and record it as `version`. The store entry is no longer needed
    /// afterwards; callers may delete it.
    public static func install(root: SandboxRoot, reference: String, version: String) async throws -> MaintenanceArtifact {
        try root.requireInitialized()
        guard !version.isEmpty, version.count <= 64, !reference.hasPrefix(Disks.initImagePrefix) else {
            throw SandboxError("invalid maintenance image \(reference) version \(version.debugDescription)")
        }
        let operation = try OperationLock.shared(root)
        defer { withExtendedLifetime(operation) {} }
        try FileManager.default.createDirectory(at: root.maintenance, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let store = try ImageStore(path: root.imageStore)
        let image = try await store.get(reference: reference)
        let manifest = try await image.manifest(for: .current)
        let capacity = try capacity(packedBytes: packedBytes(manifest.layers.map(\.size)))

        let tmp = root.maintenance.appendingPathComponent(".tmp-\(UUID().uuidString).ext4")
        defer { try? FileManager.default.removeItem(at: tmp) }
        do {
            _ = try await EXT4Unpacker(capacityInBytes: capacity, journal: .init(defaultMode: .ordered)).unpack(image, for: .current, at: tmp)
        } catch {
            throw SandboxError("unpacking maintenance image \(reference) into \(capacity) bytes failed: \(error)")
        }
        let missing = try missingPrograms(in: tmp)
        guard missing.isEmpty else {
            throw SandboxError("maintenance image \(reference) lacks \(missing.joined(separator: ", "))")
        }

        let digest = image.digest.replacingOccurrences(of: "sha256:", with: "")
        let artifact = MaintenanceArtifact(
            version: version, reference: reference, digest: image.digest, capacityBytes: capacity, installedAt: Date(),
            disk: "tools-\(digest)-\(capacity).ext4")
        let lock = try FileLock.acquire(lockURL(root), .exclusive)
        defer { withExtendedLifetime(lock) {} }
        let disk = root.maintenance.appendingPathComponent(artifact.disk)
        guard rename(tmp.path, disk.path) == 0 else { throw SandboxError("rename maintenance disk: errno \(errno)") }
        try JSONEncoder.pretty.encode(artifact).write(to: artifactURL(root), options: .atomic)
        for name in (try? FileManager.default.contentsOfDirectory(atPath: root.maintenance.path)) ?? []
        where name.hasPrefix("tools-") && name.hasSuffix(".ext4") && name != artifact.disk {
            try? FileManager.default.removeItem(at: root.maintenance.appendingPathComponent(name))
        }
        return artifact
    }

    /// Which of ``requiredPrograms`` the unpacked filesystem at `disk` lacks.
    static func missingPrograms(in disk: URL) throws -> [String] {
        let reader = try EXT4.EXT4Reader(blockDevice: FilePath(disk.path))
        return requiredPrograms.filter { !reader.exists(FilePath($0)) }
    }

    /// A private clone of the installed maintenance disk at `url`. Fails,
    /// before anything else changes, when none is installed.
    static func cloneTools(root: SandboxRoot, to url: URL) throws {
        let lock = try FileLock.acquire(lockURL(root), .shared)
        defer { withExtendedLifetime(lock) {} }
        guard let artifact = try installed(root: root) else {
            throw SandboxError(
                "no maintenance image is installed in \(root.root.path); `coop setup` installs one "
                    + "(or run `coop-sandbox maintenance install`)")
        }
        let disk = root.maintenance.appendingPathComponent(artifact.disk)
        guard FileManager.default.fileExists(atPath: disk.path) else {
            throw SandboxError("maintenance disk \(artifact.disk) is missing; reinstall it with `coop setup`")
        }
        try clone(disk, to: url)
    }

    /// Grow `disk`'s ext4 to fill its (already enlarged) file. The formatter's
    /// ext4 uses sparse_super2, which the guest kernel cannot resize online.
    public static func growFilesystem(root: SandboxRoot, scratch: URL, disk: URL) async throws -> String {
        try await run(
            root: root, scratch: scratch, target: disk, attach: .raw,
            script: "e2fsck -fy /coopdisk; rc=$?; [ $rc -le 1 ] || exit $rc; resize2fs /coopdisk")
    }

    /// Remove per-machine identity from `disk` so every sandbox cloned from it
    /// generates its own SSH host keys and machine-id on first boot.
    public static func resetIdentity(root: SandboxRoot, scratch: URL, disk: URL) async throws -> String {
        try await run(root: root, scratch: scratch, target: disk, attach: .mounted, script: resetIdentityScript)
    }

    /// Guest-controlled paths are never followed: a symlinked `/etc` or
    /// `/etc/ssh` would redirect the deletions, so it fails the reset instead,
    /// and `rm` removes a symlinked key or machine-id itself, not its target.
    static let resetIdentityScript = """
        set -e
        for d in /coopdisk/etc /coopdisk/etc/ssh; do
            if [ -L "$d" ] || [ ! -d "$d" ]; then echo "refusing: $d is not a directory" >&2; exit 3; fi
        done
        rm -f /coopdisk/etc/ssh/ssh_host_*_key /coopdisk/etc/ssh/ssh_host_*_key.pub
        rm -f /coopdisk/etc/machine-id
        : > /coopdisk/etc/machine-id
        sync
        """

    static func run(root: SandboxRoot, scratch: URL, target: URL, attach: Attach, script: String) async throws -> String {
        let toolsClone = scratch.appendingPathComponent(".maintenance-\(UUID().uuidString.prefix(8)).ext4")
        try cloneTools(root: root, to: toolsClone)
        defer { try? FileManager.default.removeItem(at: toolsClone) }

        let vmm = VZVirtualMachineManager(
            kernel: Kernel(path: root.kernel, platform: .linuxArm),
            initialFilesystem: .block(format: "ext4", source: root.initfs.path, destination: "/", options: ["ro"])
        )
        let out = BufferWriter()
        var config = LinuxContainer.Configuration()
        config.process.arguments = ["/bin/sh", "-c", script]
        config.process.capabilities = .allCapabilities
        config.process.stdout = out
        config.process.stderr = out
        config.cpus = 1
        config.memoryInBytes = 1024 * 1024 * 1024
        config.interfaces = []
        let attached: Containerization.Mount =
            switch attach {
            case .raw: .block(format: "none", source: target.path, destination: "/coopdisk", options: ["bind"])
            case .mounted: .block(format: "ext4", source: target.path, destination: "/coopdisk", options: ["nosuid", "nodev", "noexec"])
            }
        config.mounts = LinuxContainer.defaultMounts() + [attached]
        let rootfs = Containerization.Mount.block(format: "ext4", source: toolsClone.path, destination: "/", options: [])
        let container = try LinuxContainer("m-\(UUID().uuidString.prefix(12).lowercased())", rootfs: rootfs, vmm: vmm, configuration: config)
        try await container.create()
        try await container.start()
        let status = try await container.wait(timeoutInSeconds: 600)
        try? await container.stop()
        let log = String(decoding: out.data, as: UTF8.self)
        guard status.exitCode == 0 else { throw SandboxError("maintenance failed (exit \(status.exitCode)):\n\(log)") }
        return log
    }
}
