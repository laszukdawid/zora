import AppKit
import Foundation
import ZoraCore
import ZoraMac
import os

private let log = Logger(subsystem: "dev.zora.agent", category: "bridge")

/// The macOS plugin side of docs/protocol.md, in "attached" mode: this app process owns the microphone, speech,
/// dictation monitoring and notifications (so TCC sees the app's identity) and runs the Rust core `zorad` as a
/// child that talks the protocol over fds 3 (core reads) and 4 (core writes).
final class Bridge: @unchecked Sendable {
    struct Options {
        var zorad: String
        var coreArgs: [String]
        var audioFile: URL?
    }

    private let options: Options
    private let lock = NSLock()
    private var seq: UInt64 = 0 // only touched on `writer`
    private var generation: UInt64 = 0
    private var ready = false
    private var writeFD: Int32 = -1
    private var corePID: pid_t = 0
    private var stopping = false
    private var started = false
    private var pipeline: SpeechPipeline?
    private var monitor: CoreAudioDictationMonitor?
    private let notifier = MacNotifier()
    private let writer = DispatchQueue(label: "dev.zora.bridge.writer")
    private var keep: [AnyObject] = []
    /// Created on the main thread in `run()`.
    private var indicator: StatusIndicator?

    init(options: Options) { self.options = options }

    // MARK: Lifecycle

    @MainActor
    func startIndicator() {
        indicator = StatusIndicator()
    }

    func run() {
        // A windowless background listener must never be quit as "idle" by AppKit/macOS.
        ProcessInfo.processInfo.disableAutomaticTermination("Zora listens in the background")
        ProcessInfo.processInfo.disableSuddenTermination()
        // A core that closes its end must not kill the app: EPIPE is handled as connection loss instead.
        signal(SIGPIPE, SIG_IGN)
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let src = DispatchSource.makeSignalSource(signal: sig, queue: .main)
            src.setEventHandler { [weak self] in self?.requestStop() }
            src.resume()
            keep.append(src)
        }
        let center = NSWorkspace.shared.notificationCenter
        keep.append(center.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: nil) { [weak self] _ in
            self?.pipeline?.markDiscontinuity()
        } as AnyObject)
        Thread.detachNewThread { [self] in supervise() }
    }

    /// Orderly stop: the core sends `stop`, we finalize audio and answer `stopped`, then the core exits.
    func requestStop() {
        let pid: pid_t = lock.withLock { stopping = true; return corePID }
        if pid > 0 { kill(pid, SIGTERM) } else { exit(0) }
    }

    private func supervise() {
        var backoff = 1.0
        while true {
            let startedAt = Date()
            guard let pid = spawnCore() else {
                log.error("cannot start zorad at \(self.options.zorad, privacy: .public)")
                Thread.sleep(forTimeInterval: backoff)
                backoff = min(backoff * 2, 60)
                continue
            }
            var status: Int32 = 0
            while waitpid(pid, &status, 0) == -1 && errno == EINTR {}
            let (isStopping, fd): (Bool, Int32) = lock.withLock {
                ready = false
                corePID = 0
                let fd = writeFD
                writeFD = -1
                return (stopping, fd)
            }
            if fd >= 0 { close(fd) }
            if isStopping {
                Task { await self.pipeline?.stop(); exit(0) }
                Thread.sleep(forTimeInterval: 10)
                exit(0)
            }
            // A clean exit with status 0 outside a stop means the core refused to run (e.g. bad config).
            let exitedCleanly = (status & 0x7f) == 0 && ((status >> 8) & 0xff) == 0
            log.error("zorad exited (status \(status)); restarting")
            if exitedCleanly && Date().timeIntervalSince(startedAt) < 5 {
                FileHandle.standardError.write(Data("zora: zorad exited at startup; check `zorad status`\n".utf8))
                exit(0)
            }
            if Date().timeIntervalSince(startedAt) > 60 { backoff = 1 }
            Thread.sleep(forTimeInterval: backoff)
            backoff = min(backoff * 2, 60)
        }
    }

    private func spawnCore() -> pid_t? {
        var toCore: [Int32] = [0, 0], fromCore: [Int32] = [0, 0]
        guard pipe(&toCore) == 0, pipe(&fromCore) == 0 else { return nil }
        // Move all four descriptors above 9 so dup2 onto 3 and 4 never collides with an existing number.
        func high(_ fd: Int32) -> Int32 { let n = fcntl(fd, F_DUPFD_CLOEXEC, 10); close(fd); return n }
        let coreRead = high(toCore[0]), ourWrite = high(toCore[1]), ourRead = high(fromCore[0]), coreWrite = high(fromCore[1])

        var actions = posix_spawn_file_actions_t(bitPattern: 0)
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_addinherit_np(&actions, 1)
        posix_spawn_file_actions_addinherit_np(&actions, 2)
        posix_spawn_file_actions_adddup2(&actions, coreRead, 3)
        posix_spawn_file_actions_adddup2(&actions, coreWrite, 4)
        var attr = posix_spawnattr_t(bitPattern: 0)
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))
        var all = sigset_t(), none = sigset_t()
        sigfillset(&all); sigemptyset(&none)
        posix_spawnattr_setsigdefault(&attr, &all)
        posix_spawnattr_setsigmask(&attr, &none)

        let args = ["zorad", "run", "--attached-plugin"] + options.coreArgs
        let argv = args.map { strdup($0) } + [nil]
        defer { argv.forEach { free($0) } }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, options.zorad, &actions, &attr, argv, environ)
        close(coreRead); close(coreWrite)
        guard rc == 0 else { close(ourRead); close(ourWrite); return nil }
        lock.withLock { corePID = pid; writeFD = ourWrite; ready = false }
        Thread.detachNewThread { [self] in readLoop(fd: ourRead) }
        return pid
    }

    private func readLoop(fd: Int32) {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            let n = read(fd, &chunk, chunk.count)
            if n < 0 && errno == EINTR { continue }
            if n <= 0 { break }
            buffer.append(contentsOf: chunk[0..<n])
            while let nl = buffer.firstIndex(of: 0x0A) {
                let line = buffer[buffer.startIndex..<nl]
                buffer.removeSubrange(buffer.startIndex...nl)
                if let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any] { handle(obj) }
            }
            if buffer.count > 64 * 1024 { break } // protocol limit
        }
        close(fd)
        // A reader that stopped while the core still runs leaves a half-open channel: restart the core.
        let pid: pid_t = lock.withLock { stopping ? 0 : corePID }
        if pid > 0 {
            log.error("protocol channel from core closed; restarting it")
            kill(pid, SIGKILL)
        }
    }

    // MARK: Protocol

    private func handle(_ msg: [String: Any]) {
        switch msg["t"] as? String {
        case "hello":
            let gen = (msg["generation"] as? NSNumber)?.uint64Value ?? 0
            guard (msg["clock"] as? String) == "darwin-uptime-raw-ns" else { log.error("clock domain mismatch"); return }
            let config = msg["config"] as? [String: Any] ?? [:]
            let isRestart: Bool = lock.withLock {
                let restart = generation != 0
                generation = gen
                return restart
            }
            pendingConfig = config
            sendRaw(["t": "ready", "protocol": 1, "generation": gen, "capabilities": ["transcript", "dictation", "notify"],
                     "clock": "darwin-uptime-raw-ns", "now_ns": HostClock.nowNanos()])
            lock.withLock { ready = true }
            if isRestart { pipeline?.markDiscontinuity() } // sessions are scoped to a generation
        case "start":
            startCapabilities()
        case "sync":
            let id = msg["id"] as? NSNumber ?? 0
            monitor?.sync(reply: { [weak self] o in
                self?.send(["t": "synced", "id": id, "at_ns": o.atNanos, "healthy": o.healthy, "capturing": o.capturing])
            }, unknown: { [weak self] f, t in self?.send(["t": "unknown", "from_ns": f, "to_ns": t]) })
        case "notify":
            let id = msg["id"] as? NSNumber ?? 0
            let title = msg["title"] as? String ?? "Zora", body = msg["body"] as? String ?? ""
            let kind = msg["kind"] as? String ?? ""
            let sounds = (pendingConfig["sounds"] as? Bool) ?? true
            Task { @MainActor [weak self] in
                guard let indicator = self?.indicator else { return }
                indicator.soundsEnabled = sounds
                indicator.handle(kind: kind, title: title, body: body)
            }
            Task { [weak self] in
                guard let self else { return }
                let listeningID = "dev.zora.listening"
                let cancelled = kind == "captureDiscarded" && body == "cancelled"
                let ok: Bool
                if cancelled {
                    // "Stop it, Zora": forget quietly. Withdraw the listening notice; nothing else is shown.
                    notifier.withdraw(identifier: listeningID)
                    ok = true
                } else if kind == "jobStarted" {
                    // Only moves the menu-bar icon; the "got it" banner already said it is running.
                    ok = true
                } else {
                    ok = await notifier.post(title: title, body: body, identifier: kind == "captureStarted" ? listeningID : UUID().uuidString)
                }
                send(["t": "ack", "id": id, "ok": ok])
            }
        case "clock":
            send(["t": "clock", "id": msg["id"] as? NSNumber ?? 0, "now_ns": HostClock.nowNanos()])
        case "stop":
            Task { [weak self] in
                guard let self else { return }
                await pipeline?.stop()
                monitor?.stop()
                send(["t": "stopped"])
                // A later core connection starts fresh capture and a fresh monitoring snapshot.
                lock.withLock {
                    self.pipeline = nil
                    self.monitor = nil
                    self.started = false
                }
            }
        default:
            break
        }
    }

    private var pendingConfig: [String: Any] = [:]
    /// Last capture state published by the monitor (began/ended); only touched on the monitor's queue.
    private var dictating = false
    private var pendingWrites = 0

    private func startCapabilities() {
        let first: Bool = lock.withLock { defer { started = true }; return !started }
        guard first else { return }
        let config = pendingConfig
        let bundleIDs = config["bundleIDs"] as? [String] ?? ["com.typewhisper.mac"]
        let monitor = CoreAudioDictationMonitor(bundleIDs: bundleIDs) { [weak self] event in
            switch event {
            case .began(let t):
                self?.dictating = true
                self?.send(["t": "dictation", "state": "began", "at_ns": HostClock.nanos(t)])
            case .ended(let t):
                self?.dictating = false
                self?.send(["t": "dictation", "state": "ended", "at_ns": HostClock.nanos(t)])
            case .health(let ok, let reason):
                self?.send(["t": "health", "healthy": ok, "capturing": self?.dictating ?? false, "reason": reason, "at_ns": HostClock.nowNanos()])
            }
        }
        monitor.start()
        monitor.startHeartbeat(heartbeat: { [weak self] o in
            self?.send(["t": "health", "healthy": o.healthy, "capturing": o.capturing, "reason": o.healthy ? "ok" : "unknown", "at_ns": o.atNanos])
        }, unknown: { [weak self] f, t in self?.send(["t": "unknown", "from_ns": f, "to_ns": t]) })
        self.monitor = monitor

        let locale = Locale(identifier: config["locale"] as? String ?? "en-US")
        let useDetector = config["speechDetector"] as? Bool ?? true
        let pipeline = SpeechPipeline(locale: locale, useDetector: useDetector) { [weak self] event in
            switch event {
            case .segment(let s):
                self?.send(["t": "segment", "session": s.session, "start_ns": HostClock.nanos(s.start), "end_ns": HostClock.nanos(s.end), "text": s.text])
            case .gap(let reason, let at):
                self?.send(["t": "gap", "at_ns": HostClock.nanos(at), "reason": reason])
            }
        }
        if config["earlyWake"] as? Bool ?? true {
            pipeline.onPartial = { [weak self] session, text in
                log.debug("partial chars=\(text.count)")
                self?.send(["t": "partial", "session": session, "text": String(text.suffix(512))])
            }
            log.info("early wake enabled")
        }
        self.pipeline = pipeline
        Task { [weak self] in
            do {
                try await pipeline.prepare()
                self?.send(["t": "status", "ready": true, "reason": "speech ready"])
                if let file = self?.options.audioFile {
                    try await pipeline.transcribeFile(file)
                    // File mode is for testing: give hold-back and sync time, then stop the service.
                    try await Task.sleep(for: .seconds(5))
                    self?.requestStop()
                } else {
                    // First run from launchd: the prompts belong to Zora.app itself, which is what TCC remembers.
                    if Microphone.status == "not determined" { _ = await Microphone.request() }
                    if let mac = self?.notifier, mac.isAvailable, await mac.authorizationDescription() == "not determined" {
                        _ = await mac.requestAuthorization()
                    }
                    guard Microphone.granted else { throw SpeechPipelineError.microphoneUnavailable("access is \(Microphone.status); allow Zora in System Settings → Privacy & Security → Microphone") }
                    try pipeline.startMicrophone()
                }
            } catch {
                self?.send(["t": "status", "ready": false, "reason": "\(error)"])
            }
        }
    }

    // MARK: Output

    /// The writer queue is the single point that assigns `seq` and writes, so sequence order is wire order.
    private func send(_ body: [String: Any]) {
        let overloaded: Bool = lock.withLock {
            pendingWrites += 1
            return pendingWrites > 1024
        }
        if overloaded {
            // The core stopped reading. Dropping messages could hide a dictation event, so end the connection
            // instead; the supervisor restarts the core, which starts again from unknown suppression state.
            let pid: pid_t = lock.withLock { pendingWrites -= 1; return corePID }
            log.error("core is not reading; restarting it")
            if pid > 0 { kill(pid, SIGKILL) }
            return
        }
        writer.async { [self] in
            defer { lock.withLock { pendingWrites -= 1 } }
            guard let (fd, gen): (Int32, UInt64) = lock.withLock({ ready && writeFD >= 0 ? (writeFD, generation) : nil }) else { return }
            seq += 1
            var msg = body
            msg["seq"] = seq
            msg["generation"] = gen
            writeLine(msg, to: fd)
        }
    }

    private func sendRaw(_ msg: [String: Any]) {
        writer.sync { [self] in
            guard let fd: Int32 = lock.withLock({ writeFD >= 0 ? writeFD : nil }) else { return }
            seq = 0
            writeLine(msg, to: fd)
        }
    }

    /// Runs on `writer`.
    private func writeLine(_ msg: [String: Any], to fd: Int32) {
        guard var data = try? JSONSerialization.data(withJSONObject: msg) else { return }
        data.append(0x0A)
        data.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = Darwin.write(fd, raw.baseAddress! + off, raw.count - off)
                if n < 0 { if errno == EINTR { continue }; return }
                off += n
            }
        }
    }
}
