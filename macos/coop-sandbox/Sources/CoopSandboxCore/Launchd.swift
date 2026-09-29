import Foundation

/// Supervision of sandbox owners by launchd.
///
/// The VM lives inside its owner process, so each running sandbox is one
/// launchd job loaded from a plist in the sandbox directory (never in
/// `~/Library/LaunchAgents`, so nothing starts at login). launchd restarts the
/// owner whenever it exits abnormally (killed, crashed); the owner exits 0 on a
/// clean halt and on its own startup errors, so neither respawns.
public enum Launchd {
    static let launchctl = "/bin/launchctl"

    /// `gui/<uid>` when a GUI session exists, else `user/<uid>` (e.g. over SSH).
    public static func domain() -> String {
        let uid = getuid()
        return run([launchctl, "print", "gui/\(uid)"]).status == 0 ? "gui/\(uid)" : "user/\(uid)"
    }

    public static func plist(label: String, executable: String, arguments: [String], log: URL) -> [String: Any] {
        [
            "Label": label,
            "ProgramArguments": [executable] + arguments,
            "RunAtLoad": true,
            "KeepAlive": ["SuccessfulExit": false],
            "ThrottleInterval": 10,
            // The owner halts systemd on SIGTERM; give it time before SIGKILL.
            "ExitTimeOut": 90,
            "ProcessType": "Interactive",
            "StandardOutPath": log.path,
            "StandardErrorPath": log.path,
            // Nothing from the caller's environment reaches the runtime.
            "EnvironmentVariables": ["PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": NSHomeDirectory()],
        ]
    }

    public static func write(_ plist: [String: Any], to url: URL) throws {
        let data = try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
        try data.write(to: url, options: .atomic)
        chmod(url.path, 0o600)
    }

    public static func isLoaded(_ label: String, domain: String) -> Bool {
        run([launchctl, "print", "\(domain)/\(label)"]).status == 0
    }

    public static func bootstrap(plist url: URL, domain: String) throws {
        let r = run([launchctl, "bootstrap", domain, url.path])
        guard r.status == 0 else { throw SandboxError("launchctl bootstrap failed (\(r.status)): \(r.output)") }
    }

    /// Unload `label` from whichever domain holds it (a job started over SSH
    /// lives in `user/`, one started from a GUI session in `gui/`); a job that
    /// is not loaded is not an error.
    public static func bootout(_ label: String) {
        let uid = getuid()
        for domain in ["gui/\(uid)", "user/\(uid)"] where isLoaded(label, domain: domain) {
            _ = run([launchctl, "bootout", "\(domain)/\(label)"])
        }
    }

    static func run(_ argv: [String]) -> (status: Int32, output: String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: argv[0])
        p.arguments = Array(argv.dropFirst())
        p.environment = ["PATH": "/usr/bin:/bin"]
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do {
            try p.run()
        } catch {
            return (-1, "\(error)")
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus, String(decoding: data.prefix(4096), as: UTF8.self))
    }
}
