import Foundation

/// Local control protocol between CLI invocations and a sandbox's owner
/// process: one newline-terminated JSON request, one JSON response, over a
/// Unix socket in a 0700 per-user directory.
public struct ControlRequest: Codable, Sendable {
    public enum Op: String, Codable, Sendable { case ping, exec, stop, inspect }
    public var op: Op
    public var argv: [String]? = nil
    public var stdin: Data? = nil
    public var timeoutSeconds: Int64? = nil

    public init(op: Op, argv: [String]? = nil, stdin: Data? = nil, timeoutSeconds: Int64? = nil) {
        self.op = op
        self.argv = argv
        self.stdin = stdin
        self.timeoutSeconds = timeoutSeconds
    }
}

public struct ControlResponse: Codable, Sendable {
    public var ok: Bool
    public var error: String? = nil
    public var exitCode: Int32? = nil
    public var stdout: Data? = nil
    public var stderr: Data? = nil
    public var inspect: EffectiveConfig? = nil
}

/// What the running VM was actually configured with, read back from the
/// in-memory `LinuxContainer.Configuration`.
public struct EffectiveConfig: Codable, Sendable {
    public struct MountView: Codable, Sendable {
        public var type: String
        public var source: String
        public var destination: String
        public var options: [String]
    }
    public struct InterfaceView: Codable, Sendable {
        public var ipv4: String
        public var ipv4Gateway: String?
        public var ipv6: String?
        public var network: String
    }
    public var id: String
    public var imageReference: String
    public var imageDigest: String
    public var cpus: Int
    public var memoryBytes: UInt64
    public var rootfs: MountView
    public var mounts: [MountView]
    public var interfaces: [InterfaceView]
    public var socketRelays: Int
    public var publishedPorts: Int
    public var sshAgentForwarding: Bool
    public var maskedPaths: [String]
    public var readonlyPaths: [String]
    public var initArgv: [String]
    public var virtualization: Bool
}

public enum ControlSocket {
    /// Largest request line an owner reads; bounds its memory per
    /// connection. `exec -i` stdin travels inside the request.
    public static let maxRequestBytes = 8 << 20

    static func prepareDirectory(for path: URL) throws {
        let dir = path.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try verifyDirectory(for: path)
    }

    /// The socket's directory must be a real directory owned by this user and
    /// closed to everyone else, or another local user could stand in for an
    /// owner (the socket name is predictable).
    static func verifyDirectory(for path: URL) throws {
        let dir = path.deletingLastPathComponent()
        var st = stat()
        guard lstat(dir.path, &st) == 0, st.st_uid == getuid(), (st.st_mode & S_IFMT) == S_IFDIR, (st.st_mode & 0o077) == 0 else {
            throw SandboxError("control directory \(dir.path) is not a private directory owned by this user")
        }
    }

    private static func address(_ path: URL) throws -> sockaddr_un {
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else { throw SandboxError("socket path too long") }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        return addr
    }

    /// Binds and listens; `handler` runs per connection on a detached task.
    public static func serve(at path: URL, handler: @escaping @Sendable (ControlRequest) async -> ControlResponse) throws {
        try prepareDirectory(for: path)
        unlink(path.path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw SandboxError("socket: \(errno)") }
        var addr = try address(path)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard rc == 0 else { throw SandboxError("bind \(path.path): \(errno)") }
        chmod(path.path, 0o600)
        guard listen(fd, 16) == 0 else { throw SandboxError("listen: \(errno)") }
        // macOS deletes items in the per-user temp directory that go unused
        // for days; touch the socket and its directory so a long-running
        // sandbox keeps its control channel.
        Thread.detachNewThread {
            while true {
                Thread.sleep(forTimeInterval: 3600)
                utimes(path.path, nil)
                utimes(path.deletingLastPathComponent().path, nil)
            }
        }
        Thread.detachNewThread {
            while true {
                let conn = accept(fd, nil, nil)
                if conn < 0 { continue }
                // A client that went away must not kill the owner (SIGPIPE),
                // or launchd would restart a sandbox that was being stopped.
                var one: Int32 = 1
                setsockopt(conn, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
                // Only the owning user may drive the sandbox.
                var uid: uid_t = 0
                var gid: gid_t = 0
                guard getpeereid(conn, &uid, &gid) == 0, uid == getuid() else {
                    close(conn)
                    continue
                }
                let handle = FileHandle(fileDescriptor: conn, closeOnDealloc: true)
                Task.detached {
                    let response: ControlResponse
                    switch readLine(handle, limit: maxRequestBytes) {
                    case .line(let line):
                        if let request = try? JSONDecoder().decode(ControlRequest.self, from: line) {
                            response = await handler(request)
                        } else {
                            response = ControlResponse(ok: false, error: "malformed request")
                        }
                    case .tooLong:
                        response = ControlResponse(ok: false, error: "request exceeds \(maxRequestBytes) bytes")
                    case .closed:
                        response = ControlResponse(ok: false, error: "malformed request")
                    }
                    var out = (try? JSONEncoder().encode(response)) ?? Data()
                    out.append(0x0A)
                    try? handle.write(contentsOf: out)
                    try? handle.close()
                }
            }
        }
    }

    /// Sends one request. `timeout` bounds the wait for the reply (nil waits
    /// indefinitely, for a long exec); an expired wait is an error.
    public static func call(_ path: URL, _ request: ControlRequest, timeout: TimeInterval? = 30) throws -> ControlResponse {
        try verifyDirectory(for: path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw SandboxError("socket: \(errno)") }
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        if let timeout {
            var tv = timeval(tv_sec: Int(timeout), tv_usec: 0)
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        }
        var addr = try address(path)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        guard rc == 0 else { throw SandboxError("not running (connect \(path.path): errno \(errno))") }
        var data = try JSONEncoder().encode(request)
        guard data.count <= maxRequestBytes else {
            throw SandboxError("request is \(data.count) bytes; the limit is \(maxRequestBytes) (is exec stdin too large?)")
        }
        data.append(0x0A)
        try handle.write(contentsOf: data)
        guard case .line(let line) = readLine(handle, limit: nil) else {
            throw SandboxError("owner closed the connection or did not reply in time")
        }
        return try JSONDecoder().decode(ControlResponse.self, from: line)
    }

    enum ReadResult: Equatable {
        case line(Data)
        /// The peer sent more than the limit without a newline.
        case tooLong
        /// End of stream, an error, or a timeout, before any byte.
        case closed
    }

    /// Reads one request/response line of at most `limit` bytes (excluding
    /// the newline). `FileHandle.read(upToCount:)` blocks until the full
    /// count arrives, so read(2) directly.
    static func readLine(_ handle: FileHandle, limit: Int?) -> ReadResult {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 65536)
        while true {
            let n = read(handle.fileDescriptor, &chunk, chunk.count)
            if n < 0, errno == EINTR { continue }
            guard n > 0 else { return buffer.isEmpty ? .closed : .line(buffer) }
            let end = chunk[0..<n].firstIndex(of: 0x0A) ?? n
            if let limit, buffer.count + end > limit { return .tooLong }
            buffer.append(contentsOf: chunk[0..<end])
            if end < n { return .line(buffer) }
        }
    }
}
