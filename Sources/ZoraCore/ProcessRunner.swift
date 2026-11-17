import Foundation

public struct ProcessOutcome: Sendable, Equatable {
    public enum Termination: Sendable, Equatable { case exited(Int32), signaled(Int32), timedOut, cancelled, spawnFailed(Int32) }
    public var termination: Termination
    public var stdout: String
    public var stderr: String
    public var truncated: Bool
}

/// Spawns a process without a shell in its own process group, drains both pipes with a cap, and kills the
/// whole group on timeout or cancellation (SIGTERM, then SIGKILL after `killGrace`).
public enum ProcessRunner {
    public static func run(
        executable: String, arguments: [String], cwd: String, environment: [String: String],
        timeout: Double, captureLimit: Int = 64 * 1024, killGrace: Double = 5
    ) async -> ProcessOutcome {
        var outPipe: [Int32] = [0, 0], errPipe: [Int32] = [0, 0]
        guard pipe(&outPipe) == 0, pipe(&errPipe) == 0 else {
            return ProcessOutcome(termination: .spawnFailed(errno), stdout: "", stderr: "", truncated: false)
        }

        var actions = posix_spawn_file_actions_t(bitPattern: 0)
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_adddup2(&actions, outPipe[1], 1)
        posix_spawn_file_actions_adddup2(&actions, errPipe[1], 2)
        posix_spawn_file_actions_addchdir(&actions, cwd)

        var attr = posix_spawnattr_t(bitPattern: 0)
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))
        posix_spawnattr_setpgroup(&attr, 0)
        // The service ignores SIGTERM/SIGINT for its own shutdown handling; children must not inherit that.
        var allSignals = sigset_t(), noSignals = sigset_t()
        sigfillset(&allSignals)
        sigemptyset(&noSignals)
        posix_spawnattr_setsigdefault(&attr, &allSignals)
        posix_spawnattr_setsigmask(&attr, &noSignals)

        let argv = ([executable] + arguments).map { strdup($0) } + [nil]
        let envp = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer { argv.forEach { free($0) }; envp.forEach { free($0) } }

        var spawned: pid_t = 0
        let rc = posix_spawn(&spawned, executable, &actions, &attr, argv, envp)
        let pid = spawned
        close(outPipe[1]); close(errPipe[1])
        guard rc == 0 else {
            close(outPipe[0]); close(errPipe[0])
            return ProcessOutcome(termination: .spawnFailed(rc), stdout: "", stderr: "", truncated: false)
        }

        let stdoutReader = CappedReader(fd: outPipe[0], limit: captureLimit)
        let stderrReader = CappedReader(fd: errPipe[0], limit: captureLimit)
        let state = RunState()
        let exitStatus = await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<Int32, Never>) in
                DispatchQueue.global(qos: .utility).async {
                    var status: Int32 = 0
                    while waitpid(pid, &status, 0) == -1 && errno == EINTR {}
                    state.markExited()
                    cont.resume(returning: status)
                }
                DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + timeout) {
                    if state.mark(.timedOut) { terminateGroup(pid, grace: killGrace) }
                }
            }
        } onCancel: {
            if state.mark(.cancelled) { terminateGroup(pid, grace: killGrace) }
        }
        // Leftover descendants would keep the pipes open; clean up the group.
        // The job is not done until its whole process group is gone: TERM, wait, then KILL and wait.
        if !(await reapGroup(pid, grace: killGrace)) {
            FileHandle.standardError.write(Data("[zora] process group \(pid) survived SIGKILL\n".utf8))
        }
        _ = await stdoutReader.wait(timeout: 2)
        _ = await stderrReader.wait(timeout: 2)

        let termination: ProcessOutcome.Termination
        switch state.reason {
        case .timedOut: termination = .timedOut
        case .cancelled: termination = .cancelled
        default:
            if exitStatus & 0x7f == 0 { termination = .exited((exitStatus >> 8) & 0xff) }
            else { termination = .signaled(exitStatus & 0x7f) }
        }
        return ProcessOutcome(
            termination: termination, stdout: stdoutReader.text, stderr: stderrReader.text,
            truncated: stdoutReader.truncated || stderrReader.truncated
        )
    }

    /// Returns true once no process remains in the group.
    static func reapGroup(_ pgid: pid_t, grace: Double) async -> Bool {
        func gone() -> Bool { kill(-pgid, 0) == -1 && errno == ESRCH }
        if gone() { return true }
        kill(-pgid, SIGTERM)
        let termDeadline = Date().addingTimeInterval(grace)
        while Date() < termDeadline {
            if gone() { return true }
            await uncancellableSleep(0.05)
        }
        kill(-pgid, SIGKILL)
        let killDeadline = Date().addingTimeInterval(2)
        while Date() < killDeadline {
            if gone() { return true }
            await uncancellableSleep(0.02)
        }
        return gone()
    }

    /// Cleanup must keep waiting even when the job's task was cancelled (Task.sleep would return at once).
    static func uncancellableSleep(_ seconds: Double) async {
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + seconds) { c.resume() }
        }
    }

    static func terminateGroup(_ pid: pid_t, grace: Double) {
        kill(-pid, SIGTERM)
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + grace) { kill(-pid, SIGKILL) }
    }

    /// Resolves a bare executable name against `searchPath`.
    public static func resolve(_ executable: String, searchPath: [String]) -> String? {
        let expanded = expandTilde(executable)
        if expanded.contains("/") { return FileManager.default.isExecutableFile(atPath: expanded) ? expanded : nil }
        for dir in searchPath {
            let candidate = (dir as NSString).appendingPathComponent(expanded)
            if FileManager.default.isExecutableFile(atPath: candidate) { return candidate }
        }
        return nil
    }
}

private final class RunState: @unchecked Sendable {
    enum Reason { case running, exited, timedOut, cancelled }
    private let lock = NSLock()
    private(set) var reason: Reason = .running

    /// Records why the process is being stopped; returns false if it already finished or was stopped.
    func mark(_ r: Reason) -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard reason == .running else { return false }
        reason = r
        return true
    }

    func markExited() {
        lock.lock(); defer { lock.unlock() }
        if reason == .running { reason = .exited }
    }
}

/// Reads a pipe to EOF on a background queue, keeping at most `limit` bytes (the rest is discarded).
private final class CappedReader: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    private var done = false
    private var waiters: [Int: CheckedContinuation<Bool, Never>] = [:]
    private var nextWaiter = 0
    private(set) var truncated = false

    init(fd: Int32, limit: Int) {
        DispatchQueue.global(qos: .utility).async { [self] in
            var buf = [UInt8](repeating: 0, count: 16 * 1024)
            while true {
                let n = read(fd, &buf, buf.count)
                if n < 0 && errno == EINTR { continue }
                if n <= 0 { break }
                lock.lock()
                let room = max(0, limit - data.count)
                if room > 0 { data.append(contentsOf: buf[0..<min(n, room)]) }
                if n > room { truncated = true }
                lock.unlock()
            }
            close(fd)
            lock.lock()
            done = true
            let ws = waiters.values
            waiters.removeAll()
            lock.unlock()
            ws.forEach { $0.resume(returning: true) }
        }
    }

    var text: String {
        lock.lock(); defer { lock.unlock() }
        return String(decoding: data, as: UTF8.self)
    }

    func wait(timeout: Double) async -> Bool {
        await withCheckedContinuation { (cont: CheckedContinuation<Bool, Never>) in
            lock.lock()
            if done { lock.unlock(); cont.resume(returning: true); return }
            let id = nextWaiter
            nextWaiter += 1
            waiters[id] = cont
            lock.unlock()
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { [self] in
                lock.lock()
                let w = waiters.removeValue(forKey: id)
                lock.unlock()
                w?.resume(returning: false)
            }
        }
    }
}
