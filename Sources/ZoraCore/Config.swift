import Foundation

public struct ZoraConfig: Codable, Sendable, Equatable {
    public struct Phrases: Codable, Sendable, Equatable {
        public var start = ["hey zora"]
        public var end = ["do it zora", "zora do it"]
        public var cancel = ["zora cancel", "cancel zora"]
        public init() {}
    }

    public struct Capture: Codable, Sendable, Equatable {
        public var timeoutSeconds = 180.0
        public var maxGapSeconds = 30.0
        public var maxChars = 4000
        public var holdBackSeconds = 1.5
        public init() {}
    }

    public struct Dictation: Codable, Sendable, Equatable {
        public var bundleIDs = ["com.typewhisper.mac"]
        public var padSeconds = 1.0
        public init() {}
    }

    public struct Harness: Codable, Sendable, Equatable {
        /// "claude" or "argv".
        public var type = "claude"
        public var workspace = ""
        public var executable = "claude"
        /// argv harness: arguments; the element "{prompt}" is replaced by the prompt as one argument.
        public var args: [String] = []
        /// claude harness: one of default, acceptEdits, plan, dontAsk.
        public var permissionMode: String?
        public var allowedTools: [String] = []
        public var extraArgs: [String] = []
        public var env: [String: String] = [:]
        public init() {}
    }

    public struct Jobs: Codable, Sendable, Equatable {
        public var timeoutSeconds = 900.0
        public var maxQueue = 8
        public var maxAgeSeconds = 600.0
        public init() {}
    }

    public var locale = "en-US"
    public var retentionHours = 4.0
    public var dryRun = false
    public var speechDetector = true
    public var path = "/opt/homebrew/bin:/usr/local/bin:~/.local/bin:/usr/bin:/bin"
    public var phrases = Phrases()
    public var capture = Capture()
    public var dictation = Dictation()
    public var defaultHarness = "claude"
    public var harnesses: [String: Harness] = ["claude": Harness()]
    public var jobs = Jobs()
    /// Notifier id -> event kinds ("*" = all).
    public var notifications: [String: [String]] = [
        "macos": ["commandQueued", "jobSucceeded", "jobFailed", "captureDiscarded", "degraded"],
        "log": ["*"],
    ]

    public init() {}

    public static let allowedPermissionModes: Set<String> = ["acceptEdits", "auto", "dontAsk", "manual", "plan"]

    public var detectorConfig: DetectorConfig {
        DetectorConfig(
            start: phrases.start, end: phrases.end, cancel: phrases.cancel,
            captureTimeout: capture.timeoutSeconds, maxGap: capture.maxGapSeconds, maxChars: capture.maxChars
        )
    }

    public var searchPath: [String] {
        path.split(separator: ":").map { expandTilde(String($0)) }
    }

    /// Returns human-readable problems; empty means valid.
    public func validate(fileManager: FileManager = .default) -> [String] {
        var problems: [String] = []
        if Locale(identifier: locale).language.languageCode == nil { problems.append("locale '\(locale)' is not valid") }
        if retentionHours <= 0 { problems.append("retentionHours must be > 0") }
        if detectorConfig.start.isEmpty || detectorConfig.end.isEmpty { problems.append("phrases.start and phrases.end must not be empty") }
        if capture.holdBackSeconds < 0 { problems.append("capture.holdBackSeconds must be >= 0") }
        if jobs.maxQueue < 1 { problems.append("jobs.maxQueue must be >= 1") }
        guard let harness = harnesses[defaultHarness] else {
            problems.append("defaultHarness '\(defaultHarness)' is not defined in harnesses")
            return problems
        }
        for (name, h) in harnesses.sorted(by: { $0.key < $1.key }) {
            problems += Self.policyProblems(name: name, harness: h)
        }
        if !dryRun {
            var isDir: ObjCBool = false
            let ws = expandTilde(harness.workspace)
            if harness.workspace.isEmpty || !fileManager.fileExists(atPath: ws, isDirectory: &isDir) || !isDir.boolValue {
                problems.append("harnesses.\(defaultHarness).workspace must be an existing directory (got '\(harness.workspace)')")
            }
        }
        return problems
    }

    static func policyProblems(name: String, harness h: Harness) -> [String] {
        var problems: [String] = []
        let allArgs = h.args + h.extraArgs
        // Every configurable argv component goes through the bypass check.
        let everything = allArgs + h.allowedTools + [h.permissionMode ?? "", h.executable]
        if everything.contains(where: { $0.contains("dangerously-skip-permissions") || $0.contains("bypassPermissions") }) {
            problems.append("harnesses.\(name): permission-bypass flags are not allowed")
        }
        if h.allowedTools.contains(where: { $0.hasPrefix("-") || $0.isEmpty }) {
            problems.append("harnesses.\(name).allowedTools entries must be tool names, not options")
        }
        switch h.type {
        case "claude":
            if let mode = h.permissionMode, !allowedPermissionModes.contains(mode) {
                problems.append("harnesses.\(name).permissionMode '\(mode)' not in \(allowedPermissionModes.sorted())")
            }
            if allArgs.contains(where: { $0.hasPrefix("--permission-mode") }) {
                problems.append("harnesses.\(name): set permissionMode instead of passing --permission-mode")
            }
        case "argv":
            if !h.args.contains("{prompt}") { problems.append("harnesses.\(name).args must contain \"{prompt}\"") }
        default:
            problems.append("harnesses.\(name).type '\(h.type)' must be 'claude' or 'argv'")
        }
        return problems
    }

    // MARK: Codable with defaults for missing keys

    enum CodingKeys: String, CodingKey {
        case locale, retentionHours, dryRun, speechDetector, path, phrases, capture, dictation, defaultHarness, harnesses, jobs, notifications
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = ZoraConfig()
        locale = try c.decodeIfPresent(String.self, forKey: .locale) ?? d.locale
        retentionHours = try c.decodeIfPresent(Double.self, forKey: .retentionHours) ?? d.retentionHours
        dryRun = try c.decodeIfPresent(Bool.self, forKey: .dryRun) ?? d.dryRun
        speechDetector = try c.decodeIfPresent(Bool.self, forKey: .speechDetector) ?? d.speechDetector
        path = try c.decodeIfPresent(String.self, forKey: .path) ?? d.path
        phrases = try c.decodeIfPresent(Phrases.self, forKey: .phrases) ?? d.phrases
        capture = try c.decodeIfPresent(Capture.self, forKey: .capture) ?? d.capture
        dictation = try c.decodeIfPresent(Dictation.self, forKey: .dictation) ?? d.dictation
        defaultHarness = try c.decodeIfPresent(String.self, forKey: .defaultHarness) ?? d.defaultHarness
        harnesses = try c.decodeIfPresent([String: Harness].self, forKey: .harnesses) ?? d.harnesses
        jobs = try c.decodeIfPresent(Jobs.self, forKey: .jobs) ?? d.jobs
        notifications = try c.decodeIfPresent([String: [String]].self, forKey: .notifications) ?? d.notifications
    }

    public static func load(from url: URL) throws -> ZoraConfig {
        try JSONDecoder().decode(ZoraConfig.self, from: Data(contentsOf: url))
    }

    public func write(to url: URL) throws {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try enc.encode(self).write(to: url, options: .atomic)
    }

    public static var defaultURL: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".config/zora/config.json")
    }
}

// Nested structs: decode with defaults too.
extension ZoraConfig.Phrases {
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init()
        start = try c.decodeIfPresent([String].self, forKey: .start) ?? start
        end = try c.decodeIfPresent([String].self, forKey: .end) ?? end
        cancel = try c.decodeIfPresent([String].self, forKey: .cancel) ?? cancel
    }
    enum CodingKeys: String, CodingKey { case start, end, cancel }
}

extension ZoraConfig.Capture {
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init()
        timeoutSeconds = try c.decodeIfPresent(Double.self, forKey: .timeoutSeconds) ?? timeoutSeconds
        maxGapSeconds = try c.decodeIfPresent(Double.self, forKey: .maxGapSeconds) ?? maxGapSeconds
        maxChars = try c.decodeIfPresent(Int.self, forKey: .maxChars) ?? maxChars
        holdBackSeconds = try c.decodeIfPresent(Double.self, forKey: .holdBackSeconds) ?? holdBackSeconds
    }
    enum CodingKeys: String, CodingKey { case timeoutSeconds, maxGapSeconds, maxChars, holdBackSeconds }
}

extension ZoraConfig.Dictation {
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init()
        bundleIDs = try c.decodeIfPresent([String].self, forKey: .bundleIDs) ?? bundleIDs
        padSeconds = try c.decodeIfPresent(Double.self, forKey: .padSeconds) ?? padSeconds
    }
    enum CodingKeys: String, CodingKey { case bundleIDs, padSeconds }
}

extension ZoraConfig.Harness {
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init()
        type = try c.decodeIfPresent(String.self, forKey: .type) ?? type
        workspace = try c.decodeIfPresent(String.self, forKey: .workspace) ?? workspace
        executable = try c.decodeIfPresent(String.self, forKey: .executable) ?? (type == "claude" ? executable : "")
        args = try c.decodeIfPresent([String].self, forKey: .args) ?? args
        permissionMode = try c.decodeIfPresent(String.self, forKey: .permissionMode)
        allowedTools = try c.decodeIfPresent([String].self, forKey: .allowedTools) ?? allowedTools
        extraArgs = try c.decodeIfPresent([String].self, forKey: .extraArgs) ?? extraArgs
        env = try c.decodeIfPresent([String: String].self, forKey: .env) ?? env
    }
    enum CodingKeys: String, CodingKey { case type, workspace, executable, args, permissionMode, allowedTools, extraArgs, env }
}

extension ZoraConfig.Jobs {
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init()
        timeoutSeconds = try c.decodeIfPresent(Double.self, forKey: .timeoutSeconds) ?? timeoutSeconds
        maxQueue = try c.decodeIfPresent(Int.self, forKey: .maxQueue) ?? maxQueue
        maxAgeSeconds = try c.decodeIfPresent(Double.self, forKey: .maxAgeSeconds) ?? maxAgeSeconds
    }
    enum CodingKeys: String, CodingKey { case timeoutSeconds, maxQueue, maxAgeSeconds }
}

public func expandTilde(_ path: String) -> String {
    (path as NSString).expandingTildeInPath
}
