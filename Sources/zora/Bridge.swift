import AppKit
import Foundation
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

    private var cli: ZoradCLI {
        let args = options.coreArgs
        let config = args.firstIndex(of: "--config").flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
        return ZoradCLI(path: options.zorad, configPath: config)
    }

    @MainActor
    func startIndicator() {
        indicator = StatusIndicator(
            cli: cli,
            pauseMic: { [weak self] in self?.setLocalPause(true) },
            restoreMic: { [weak self] in self?.setLocalPause(false) },
            settingsSaved: { [weak self] in self?.recheck.signal() }
        )
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
            coreLost()
            let refusedConfig = (status & 0x7f) == 0 && ((status >> 8) & 0xff) == Self.exitConfig
            if refusedConfig {
                waitForValidConfig()
                backoff = 1
                continue
            }
            log.error("zorad exited (status \(status)); restarting")
            if Date().timeIntervalSince(startedAt) > 60 { backoff = 1 }
            Thread.sleep(forTimeInterval: backoff)
            backoff = min(backoff * 2, 60)
        }
    }

    /// `zorad run` exits with EX_CONFIG when the config is invalid.
    private static let exitConfig: Int32 = 78
    /// Signalled when Settings saved a change, so a fixed config starts the core at once.
    private let recheck = DispatchSemaphore(value: 0)

    /// The core refused the config. The app stays in the menu bar showing the problem (Settings can fix most
    /// problems) and starts the core again once `zorad settings` reports none.
    private func waitForValidConfig() {
        log.error("zorad refused the config; waiting for it to be fixed")
        let cli = self.cli
        while true {
            // Checked the way `zorad run` will check it, so a dry run doesn't wait for a project folder.
            let dryRun = options.coreArgs.contains("--dry-run")
            let problems = Self.blocking { () -> [String] in
                do { return try await cli.settings(dryRun: dryRun).problems } catch { return ["\(error)"] }
            }
            let stop = lock.withLock { stopping }
            if stop { exit(0) }
            if problems.isEmpty { break }
            Task { @MainActor [weak self] in self?.indicator?.setNeedsSetup(problems) }
            // Polling also catches fixes made outside Settings, such as a project folder that comes back.
            _ = recheck.wait(timeout: .now() + 5)
            while recheck.wait(timeout: .now()) == .success {}
        }
        log.info("config is valid again; starting the core")
        Task { @MainActor [weak self] in self?.indicator?.setNeedsSetup(nil) }
    }

    /// Runs async work from the supervisor thread.
    private static func blocking<T: Sendable>(_ work: @escaping @Sendable () async -> T) -> T {
        let done = DispatchSemaphore(value: 0)
        let box = OSAllocatedUnfairLock<T?>(initialState: nil)
        Task {
            let v = await work()
            box.withLock { $0 = v }
            done.signal()
        }
        done.wait()
        return box.withLock { $0! }
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
        let conn: UInt64 = lock.withLock {
            corePID = pid; writeFD = ourWrite; ready = false
            connection += 1
            return connection
        }
        Thread.detachNewThread { [self] in readLoop(fd: ourRead, pid: pid, conn: conn) }
        return pid
    }

    /// `conn` identifies this core connection: messages still buffered from a core that has gone are ignored.
    private func readLoop(fd: Int32, pid myPID: pid_t, conn: UInt64) {
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
                if let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any] { handle(obj, conn: conn) }
            }
            if buffer.count > 64 * 1024 { break } // protocol limit
        }
        close(fd)
        // A reader that stopped while its core still runs leaves a half-open channel: restart that core (never a
        // newer one).
        let pid: pid_t = lock.withLock { stopping || corePID != myPID ? 0 : myPID }
        if pid > 0 {
            log.error("protocol channel from core closed; restarting it")
            kill(pid, SIGKILL)
        }
    }

    // MARK: Protocol

    private func handle(_ msg: [String: Any], conn: UInt64) {
        // Lines still buffered from a core that has gone must not touch the mic or the new connection.
        guard lock.withLock({ conn == connection }) else { return }
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
            listeningChanged(msg, conn: conn)
            startCapabilities()
        case "listening":
            listeningChanged(msg, conn: conn)
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
            onMic { [weak self] in
                guard let self else { return }
                micReady = false
                micRunning = false
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

    // MARK: Microphone on/off

    /// Start, pause, resume and stop of the microphone run one after another in this chain, in message order,
    /// so a late resume can never reopen the mic after a pause or a stop.
    private var micTail: Task<Void, Never>?
    // Under `lock`, and set the moment an event arrives (not when the chain gets to it), so a slow start-up
    // in the chain can never open the mic after a pause or a disconnect. The mic is open only while a core is
    // connected and says "listening", and no pause from the menu is waiting for the core to confirm it.
    /// nil while no core is connected (exited, restarting), so a pause saved meanwhile is never ignored.
    private var coreListening: Bool?
    private var coreUntil: Date?
    private var localPause = false
    /// Counts core connections; see `readLoop`.
    private var connection: UInt64 = 0
    private var micWanted: Bool { lock.withLock { coreListening == true && !localPause } }
    // Touched only inside the chain.
    private var micReady = false
    private var micRunning = false

    private func onMic(_ work: @escaping @Sendable () async -> Void) {
        lock.withLock {
            let previous = micTail
            micTail = Task { await previous?.value; await work() }
        }
    }

    private func listeningChanged(_ msg: [String: Any], conn: UInt64) {
        let on = msg["listening"] as? Bool ?? true
        let until = (msg["until"] as? NSNumber).map { Date(timeIntervalSince1970: $0.doubleValue) }
        let current: Bool = lock.withLock {
            guard conn == connection else { return false }
            coreListening = on
            coreUntil = until
            if !on { localPause = false } // the core has it now
            return true
        }
        guard current else { return }
        log.info("listening \(on ? "on" : "paused", privacy: .public)")
        Task { @MainActor [weak self] in self?.indicator?.setListening(on, until: until) }
        onMic { [weak self] in await self?.applyMic() }
    }

    /// The menu's "Pause" closes the mic at once, without waiting for the core (which may be restarting), and
    /// keeps it closed until the core confirms; `false` withdraws it (the pause surely wasn't saved, or the user
    /// resumed).
    private func setLocalPause(_ on: Bool) {
        let core: (Bool?, Date?) = lock.withLock {
            localPause = on
            return (coreListening, coreUntil)
        }
        // Withdrawn: the menu shows the core's state again (it may have shown a pause the core never got).
        if !on, let listening = core.0 {
            Task { @MainActor [weak self] in self?.indicator?.setListening(listening, until: core.1) }
        }
        onMic { [weak self] in await self?.applyMic() }
    }

    /// The core connection ended: close the mic until a new core says to listen.
    private func coreLost() {
        lock.withLock {
            coreListening = nil
            connection += 1
        }
        onMic { [weak self] in await self?.applyMic() }
    }

    /// Runs in the mic chain.
    private func applyMic() async {
        guard micReady, let pipeline else { return }
        if micWanted && !micRunning {
            do {
                try pipeline.startMicrophone()
                micRunning = true
            } catch {
                send(["t": "status", "ready": false, "reason": "\(error)"])
            }
        } else if !micWanted && micRunning {
            await pipeline.pauseMicrophone()
            micRunning = false
        }
    }
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
        if let file = options.audioFile {
            // File mode is for testing and ignores pause.
            Task { [weak self] in
                do {
                    try await pipeline.prepare()
                    self?.send(["t": "status", "ready": true, "reason": "speech ready"])
                    try await pipeline.transcribeFile(file)
                    // Give hold-back and sync time, then stop the service.
                    try await Task.sleep(for: .seconds(5))
                    self?.requestStop()
                } catch {
                    self?.send(["t": "status", "ready": false, "reason": "\(error)"])
                }
            }
            return
        }
        onMic { [weak self] in
            guard let self else { return }
            do {
                try await pipeline.prepare()
                send(["t": "status", "ready": true, "reason": "speech ready"])
                // First run from launchd: the prompts belong to Zora.app itself, which is what TCC remembers.
                if Microphone.status == "not determined" { _ = await Microphone.request() }
                if notifier.isAvailable, await notifier.authorizationDescription() == "not determined" {
                    _ = await notifier.requestAuthorization()
                }
                guard Microphone.granted else { throw SpeechPipelineError.microphoneUnavailable("access is \(Microphone.status); allow Zora in System Settings → Privacy & Security → Microphone") }
                micReady = true
                await applyMic()
            } catch {
                send(["t": "status", "ready": false, "reason": "\(error)"])
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
