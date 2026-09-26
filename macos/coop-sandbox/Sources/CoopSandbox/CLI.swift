import ArgumentParser
import Containerization
import CoopSandboxCore
import Foundation

@main
struct CoopSandbox: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "coop-sandbox",
        abstract: "coop's macOS sandbox runtime: persistent Linux VMs on apple/containerization.",
        subcommands: [
            Version.self, Init.self, ImageCommand.self, Create.self, Start.self, Run.self, Stop.self, Exec.self,
            Inspect.self, List.self, Set.self, Grow.self, Commit.self, Restore.self, DiskCommand.self, MaintenanceCommand.self,
            Logs.self, Delete.self, Reconcile.self,
        ]
    )

    /// Everything this binary writes (records, disks, logs, the owner it
    /// runs under launchd) is for this user alone.
    static func main() async {
        umask(0o077)
        await Self.main(nil)
    }
}

struct RootOptions: ParsableArguments {
    @Option(help: "Absolute path of the runtime state root")
    var root: String

    func resolve() throws -> SandboxRoot { try SandboxRoot(root) }
}

let mib: UInt64 = 1024 * 1024
let gib: UInt64 = 1024 * mib

struct Version: ParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Print version and protocol as JSON.")

    struct Info: Encodable {
        let name = "coop-sandbox"
        let version = runtimeVersion
        let `protocol` = protocolVersion
        let containerization = containerizationVersion
    }

    func run() throws { try printJSON(Info()) }
}

struct Init: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Create the state root: pinned kernel and init filesystem.")
    static let initImage = "\(Disks.initImagePrefix):\(containerizationVersion)"
    static let initImageDigest = "sha256:aa6ab59d0938f7fadb54ac27e80959bdd2f1dafa8050011086d5f8ab1350fd6c"

    @OptionGroup var root: RootOptions
    @Option(help: "Linux kernel image; its sha256 must be one this runtime was validated with") var kernel: String

    func run() async throws {
        let r = try root.resolve()
        try r.createDirectories()
        let (data, sha, install) = try KernelPin.select(root: r, requested: kernel)
        if install {
            try data.write(to: r.kernel, options: .atomic)
        }
        let store = try ImageStore(path: r.imageStore)
        if !FileManager.default.fileExists(atPath: r.initfs.path) {
            let image = try await store.getInitImage(reference: Self.initImage)
            let digest = try await store.get(reference: Self.initImage).digest
            guard digest == Self.initImageDigest else {
                throw SandboxError("\(Self.initImage) resolved to \(digest), expected \(Self.initImageDigest)")
            }
            let tmp = r.root.appendingPathComponent(".initfs-\(UUID().uuidString).ext4")
            defer { try? FileManager.default.removeItem(at: tmp) }
            _ = try await image.initBlock(at: tmp, for: .linuxArm)
            guard rename(tmp.path, r.initfs.path) == 0 else { throw SandboxError("rename initfs: errno \(errno)") }
        }
        try printJSON(["kernelSha256": sha, "initImage": Self.initImageDigest, "root": r.root.path])
    }
}

struct ImageCommand: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "image", abstract: "Manage OCI images in the private store.",
        subcommands: [Import.self, ListImages.self, DeleteImage.self])

    struct Import: AsyncParsableCommand {
        static let configuration = CommandConfiguration(abstract: "Load an OCI archive (e.g. from `container image save`).")
        @OptionGroup var root: RootOptions
        @Option var ociTar: String
        func run() async throws {
            let r = try root.resolve()
            try r.requireInitialized()
            try printJSON(try await Disks.importImage(root: r, ociTar: URL(fileURLWithPath: ociTar)))
        }
    }

    struct ListImages: AsyncParsableCommand {
        static let configuration = CommandConfiguration(commandName: "list")
        @OptionGroup var root: RootOptions
        func run() async throws { try printJSON(try await Disks.listImages(root: try root.resolve())) }
    }

    struct DeleteImage: AsyncParsableCommand {
        static let configuration = CommandConfiguration(commandName: "delete")
        @OptionGroup var root: RootOptions
        @Argument var reference: String
        func run() async throws { try await Disks.deleteImage(root: try root.resolve(), reference: reference) }
    }
}

struct Create: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Create a stopped sandbox with an explicitly sized disk.")
    @OptionGroup var root: RootOptions
    @Argument var id: String
    @Option(help: "OCI image reference in the private store") var image: String?
    @Option(help: "Committed disk to clone instead of an image") var fromDisk: String?
    @Option var cpus: Int
    @Option var memoryMib: UInt64
    @Option var diskGib: UInt64
    @Option(help: "Ownership tag; `delete` requires it to match") var owner: String

    func validate() throws {
        guard (image == nil) != (fromDisk == nil) else { throw ValidationError("pass exactly one of --image or --from-disk") }
    }

    func run() async throws {
        let source: SandboxSource = if let image { .image(image) } else { .disk(try SandboxID(fromDisk ?? "")) }
        let record = try await Sandboxes.create(
            root: try root.resolve(), id: try SandboxID(id), owner: owner, source: source, cpus: cpus,
            memoryBytes: memoryMib * mib, diskBytes: diskGib * gib)
        try printJSON(record)
    }
}

struct Start: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Boot a sandbox under launchd and wait for its control channel.")
    @OptionGroup var root: RootOptions
    @Argument var id: String
    @Option var waitSeconds = 120

    func run() async throws {
        let exe = Bundle.main.executablePath ?? CommandLine.arguments[0]
        let live = try await Sandboxes.start(root: try root.resolve(), id: try SandboxID(id), executable: exe, wait: TimeInterval(waitSeconds))
        try printJSON(live)
    }
}

struct Run: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Own a sandbox VM in the foreground (launchd runs this).", shouldDisplay: false)
    @OptionGroup var root: RootOptions
    @Argument var id: String
    func run() async throws {
        let paths = try root.resolve().sandbox(try SandboxID(id))
        do {
            try await Owner.run(root: try root.resolve(), id: try SandboxID(id))
        } catch {
            // Exit 0 so launchd does not respawn a configuration error; `start`
            // reports the marker instead.
            try? Data("\(error)".utf8).write(to: paths.ownerFailed, options: .atomic)
            FileHandle.standardError.write(Data("owner failed: \(error)\n".utf8))
        }
    }
}

struct Stop: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Halt systemd cleanly and wait for the owner to exit.")
    @OptionGroup var root: RootOptions
    @Argument var id: String
    @Option var timeoutSeconds = 90
    func run() async throws {
        try await Sandboxes.stop(root: try root.resolve(), id: try SandboxID(id), timeout: TimeInterval(timeoutSeconds))
    }
}

struct Exec: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Run a command as root in the sandbox over the native control path.")
    @OptionGroup var root: RootOptions
    @Flag(name: .shortAndLong, help: "Forward this process's stdin") var interactive = false
    @Option var timeout: Int64?
    @Argument var id: String
    @Argument(parsing: .captureForPassthrough) var argv: [String] = []

    func run() async throws {
        let paths = try root.resolve().sandbox(try SandboxID(id))
        let command = argv.first == "--" ? Array(argv.dropFirst()) : argv
        guard !command.isEmpty else { throw ValidationError("no command given") }
        let input = interactive ? FileHandle.standardInput.readDataToEndOfFile() : nil
        let response = try ControlSocket.call(
            paths.control, .init(op: .exec, argv: command, stdin: input, timeoutSeconds: timeout),
            timeout: timeout.map { TimeInterval($0) + 30 })
        guard response.ok else { throw SandboxError(response.error ?? "exec failed") }
        if let out = response.stdout { FileHandle.standardOutput.write(out) }
        if let err = response.stderr { FileHandle.standardError.write(err) }
        throw ExitCode(response.exitCode ?? 1)
    }
}

struct Inspect: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Record, status, disk allocation, and (when running) the effective VM config.")
    @OptionGroup var root: RootOptions
    @Argument var id: String
    func run() async throws { try printJSON(try Sandboxes.inspect(root: try root.resolve(), id: try SandboxID(id))) }
}

struct List: AsyncParsableCommand {
    @OptionGroup var root: RootOptions
    func run() async throws {
        let r = try root.resolve()
        try printJSON(try r.allRecords().map {
            ["id": $0.id.rawValue, "status": Sandboxes.status(r.sandbox($0.id)).rawValue, "owner": $0.owner]
        })
    }
}

/// `--operation`: the caller's id for a `set`, `grow`, or `restore`,
/// recorded as `record.lastOperation` when it commits.
struct OperationOptions: ParsableArguments {
    @Option(help: "Operation id to record as record.lastOperation (default: generated)")
    var operation: String?

    func resolve() throws -> OperationID? { try operation.map(OperationID.init) }
}

struct Set: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Change CPU/memory of a stopped sandbox; applied at the next start.")
    @OptionGroup var root: RootOptions
    @OptionGroup var operation: OperationOptions
    @Argument var id: String
    @Option var cpus: Int?
    @Option var memoryMib: UInt64?
    @Option(help: "Refuse unless record.lastOperation is this operation") var expectOperation: String?
    func run() async throws {
        try printJSON(
            try await Sandboxes.setResources(
                root: try root.resolve(), id: try SandboxID(id), cpus: cpus, memoryBytes: memoryMib.map { $0 * mib },
                operation: try operation.resolve(), expect: try expectOperation.map(OperationID.init)))
    }
}

struct Grow: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Grow a stopped sandbox's disk and filesystem (offline).")
    @OptionGroup var root: RootOptions
    @OptionGroup var operation: OperationOptions
    @Argument var id: String
    @Option var diskGib: UInt64
    func run() async throws {
        try printJSON(
            try await Sandboxes.grow(root: try root.resolve(), id: try SandboxID(id), diskBytes: diskGib * gib, operation: try operation.resolve()))
    }
}

struct Commit: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Save a stopped sandbox's disk as a named disk, without its identity.")
    @OptionGroup var root: RootOptions
    @Argument var id: String
    @Argument var name: String
    @Flag(help: "Replace an existing disk of that name") var replace = false
    func run() async throws {
        try printJSON(try await Sandboxes.commit(root: try root.resolve(), id: try SandboxID(id), name: try SandboxID(name), replace: replace))
    }
}

struct Restore: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Replace a stopped sandbox's disk with a named disk or a fresh image copy.")
    @OptionGroup var root: RootOptions
    @OptionGroup var operation: OperationOptions
    @Argument var id: String
    @Argument(help: "Committed disk to restore from") var name: String?
    @Option(help: "Reset to a fresh copy of this image instead") var image: String?

    func validate() throws {
        guard (name == nil) != (image == nil) else { throw ValidationError("pass exactly one of NAME or --image") }
    }

    func run() async throws {
        let source: SandboxSource = if let image { .image(image) } else { .disk(try SandboxID(name ?? "")) }
        try printJSON(try await Sandboxes.restore(root: try root.resolve(), id: try SandboxID(id), source: source, operation: try operation.resolve()))
    }
}

struct DiskCommand: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "disk", abstract: "Manage committed disks.", subcommands: [ListDisks.self, DeleteDisk.self])

    struct ListDisks: AsyncParsableCommand {
        static let configuration = CommandConfiguration(commandName: "list")
        @OptionGroup var root: RootOptions
        func run() async throws { try printJSON(Disks.listDisks(root: try root.resolve())) }
    }

    struct DeleteDisk: AsyncParsableCommand {
        static let configuration = CommandConfiguration(commandName: "delete")
        @OptionGroup var root: RootOptions
        @Argument var name: String
        func run() async throws { try await Sandboxes.deleteDisk(root: try root.resolve(), name: try SandboxID(name)) }
    }
}

struct MaintenanceCommand: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "maintenance", abstract: "Manage the image disk maintenance VMs boot from.",
        subcommands: [InstallMaintenance.self, InspectMaintenance.self])

    struct InstallMaintenance: AsyncParsableCommand {
        static let configuration = CommandConfiguration(
            commandName: "install", abstract: "Unpack an imported image as the maintenance disk, kept apart from the image store.")
        @OptionGroup var root: RootOptions
        @Option(help: "Image reference in the private store") var image: String
        @Option(help: "Version of the image's recipe, reported back by `inspect`") var version: String
        func run() async throws {
            try printJSON(try await Maintenance.install(root: try root.resolve(), reference: image, version: version))
        }
    }

    struct InspectMaintenance: AsyncParsableCommand {
        static let configuration = CommandConfiguration(commandName: "inspect", abstract: "The installed maintenance artifact, or null.")
        @OptionGroup var root: RootOptions
        func run() async throws { try printJSON(try Maintenance.installed(root: try root.resolve())) }
    }
}

struct Logs: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Print the sandbox console log; --follow streams new lines.")
    @OptionGroup var root: RootOptions
    @Argument var id: String
    @Option(name: .customShort("n"), help: "Number of trailing lines") var lines: Int?
    @Flag var follow = false

    func run() async throws {
        let paths = try root.resolve().sandbox(try SandboxID(id))
        _ = try paths.loadRecord()
        let url = paths.bootLog
        var offset: UInt64 = 0
        if let lines {
            // Bounded: a tail never reads more than the log's last 256 KiB.
            offset = (try? FileManager.default.attributesOfItem(atPath: url.path)[.size] as? UInt64) ?? 0
            FileHandle.standardOutput.write(Sandboxes.tailData(url, lines: lines))
        } else {
            let initial = (try? Data(contentsOf: url)) ?? Data()
            offset = UInt64(initial.count)
            FileHandle.standardOutput.write(initial)
        }
        while follow {
            try await Task.sleep(for: .milliseconds(500))
            guard let handle = try? FileHandle(forReadingFrom: url) else { continue }
            // The log restarts at its size cap; follow it from the top.
            if let size = try? handle.seekToEnd(), size < offset { offset = 0 }
            try handle.seek(toOffset: offset)
            if let data = try handle.readToEnd(), !data.isEmpty {
                FileHandle.standardOutput.write(data)
                offset += UInt64(data.count)
            }
            try handle.close()
        }
    }
}

struct Delete: AsyncParsableCommand {
    @OptionGroup var root: RootOptions
    @Argument var id: String
    @Option(help: "Refuse unless the record's owner matches") var owner: String
    func run() async throws { try await Sandboxes.delete(root: try root.resolve(), id: try SandboxID(id), owner: owner) }
}

struct Reconcile: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        abstract: "Clear crashed owners and finish interrupted creates, deletes, and disk updates.")
    @OptionGroup var root: RootOptions
    func run() async throws { try printJSON(try Sandboxes.reconcile(root: try root.resolve())) }
}
