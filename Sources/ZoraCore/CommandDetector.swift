import Foundation

/// One normalized token plus the original text it came from, so prompts can be sliced at exact token
/// boundaries ("Zora,fix" → "Zora," + "fix") and rebuilt with their original spacing and punctuation.
public struct TextUnit: Sendable, Equatable {
    public var norm: String
    /// Whitespace and opening punctuation before the token.
    public var leading: String
    /// The token as written plus any trailing punctuation attached to it.
    public var original: String
}

public enum Normalizer {
    /// Lowercased, diacritic/width-folded word tokens. Punctuation separates tokens; apostrophes are dropped.
    public static func tokens(_ text: String) -> [String] { units(text).map(\.norm) }

    public static func units(_ text: String) -> [TextUnit] {
        var out: [TextUnit] = []
        var leading = "", token = "", trailing = ""

        func finish() {
            guard !token.isEmpty else { return }
            let norm = fold(token)
            if norm.isEmpty {
                leading += token + trailing
            } else {
                out.append(TextUnit(norm: norm, leading: leading, original: token + trailing))
                leading = ""
            }
            token = ""; trailing = ""
        }

        for ch in text {
            if ch.isLetter || ch.isNumber || ((ch == "'" || ch == "\u{2019}") && !token.isEmpty && trailing.isEmpty) {
                if !trailing.isEmpty { finish() }
                token.append(ch)
            } else if ch.isWhitespace {
                finish()
                if !trailing.isEmpty { trailing = "" }
                leading.append(ch)
            } else if token.isEmpty {
                leading.append(ch)
            } else {
                trailing.append(ch)
            }
        }
        finish()
        return out
    }

    static func fold(_ s: String) -> String {
        let folded = s.folding(options: [.caseInsensitive, .diacriticInsensitive, .widthInsensitive], locale: nil)
        return String(String.UnicodeScalarView(folded.unicodeScalars.filter { CharacterSet.alphanumerics.contains($0) }))
    }
}

public struct DetectorConfig: Sendable, Equatable {
    public var start: [[String]]
    public var end: [[String]]
    public var cancel: [[String]]
    public var captureTimeout: Double
    public var maxGap: Double
    public var maxChars: Int
    public var idleTailTokens: Int
    public var idleTailTTL: Double

    public init(
        start: [String], end: [String], cancel: [String],
        captureTimeout: Double = 180, maxGap: Double = 30, maxChars: Int = 4000,
        idleTailTokens: Int = 6, idleTailTTL: Double = 3
    ) {
        self.start = start.map(Normalizer.tokens).filter { !$0.isEmpty }
        self.end = end.map(Normalizer.tokens).filter { !$0.isEmpty }
        self.cancel = cancel.map(Normalizer.tokens).filter { !$0.isEmpty }
        self.captureTimeout = captureTimeout
        self.maxGap = maxGap
        self.maxChars = maxChars
        self.idleTailTokens = idleTailTokens
        self.idleTailTTL = idleTailTTL
    }
}

public enum DetectorOutput: Sendable, Equatable {
    case captureStarted(at: HostSeconds)
    case command(Command)
    case discarded(DiscardReason)
}

/// Pure state machine that extracts commands delimited by start/end phrases from final transcript segments.
public struct CommandDetector: Sendable {
    struct Unit: Sendable {
        var text: TextUnit
        var time: HostSeconds
    }

    public let config: DetectorConfig
    private var units: [Unit] = []
    private var capturing = false
    private var captureStart: HostSeconds = 0
    private var lastActivity: HostSeconds = 0

    public init(config: DetectorConfig) { self.config = config }

    public var isCapturing: Bool { capturing }
    public var captureStartedAt: HostSeconds? { capturing ? captureStart : nil }

    public mutating func feed(_ segment: TranscriptSegment) -> [DetectorOutput] {
        var out: [DetectorOutput] = []
        if capturing, segment.start - lastActivity > config.maxGap {
            out += abort(.gap)
        }
        if !capturing, let last = units.last, segment.start - last.time > config.idleTailTTL {
            units.removeAll()
        }
        var parts = Normalizer.units(segment.text)
        if !units.isEmpty, let first = parts.first, first.leading.isEmpty { parts[0].leading = " " }
        // Spread token times across the segment so a phrase start gets a sensible timestamp.
        let step = parts.isEmpty ? 0 : (segment.end - segment.start) / Double(parts.count)
        for (i, p) in parts.enumerated() {
            units.append(Unit(text: p, time: segment.start + step * Double(i)))
        }
        lastActivity = max(lastActivity, segment.end)
        out += scan()
        return out
    }

    /// Ends an open capture (dictation start, gap, session restart). When idle it only clears the tail.
    public mutating func abort(_ reason: DiscardReason) -> [DetectorOutput] {
        let wasCapturing = capturing
        capturing = false
        units.removeAll()
        return wasCapturing ? [.discarded(reason)] : []
    }

    /// Drives time-based expiry even when no speech arrives.
    public mutating func tick(now: HostSeconds) -> [DetectorOutput] {
        guard capturing else {
            if let last = units.last, now - last.time > config.idleTailTTL + 1 { units.removeAll() }
            return []
        }
        if now - captureStart > config.captureTimeout { return abort(.timeout) }
        if now - lastActivity > config.maxGap { return abort(.gap) }
        return []
    }

    private mutating func scan() -> [DetectorOutput] {
        var out: [DetectorOutput] = []
        while true {
            if !capturing {
                guard let m = firstMatch(of: config.start) else {
                    if units.count > config.idleTailTokens { units.removeFirst(units.count - config.idleTailTokens) }
                    return out
                }
                capturing = true
                captureStart = units[m.first].time
                units.removeFirst(m.last + 1)
                out.append(.captureStarted(at: captureStart))
                continue
            }

            let end = firstMatch(of: config.end)
            let cancel = firstMatch(of: config.cancel)
            if let c = cancel, end.map({ c.first < $0.first }) ?? true {
                capturing = false
                units.removeFirst(c.last + 1)
                out.append(.discarded(.cancelled))
                continue
            }
            if let e = end {
                let text = Self.clean(units[..<e.first].map { $0.text.leading + $0.text.original }.joined())
                let endTime = units[e.last].time
                capturing = false
                units.removeFirst(e.last + 1)
                // Limits apply even when both delimiters arrive in one segment.
                if text.isEmpty {
                    out.append(.discarded(.empty))
                } else if text.count > config.maxChars {
                    out.append(.discarded(.tooLong))
                } else if endTime - captureStart > config.captureTimeout {
                    out.append(.discarded(.timeout))
                } else {
                    out.append(.command(Command(text: text, captureStart: captureStart, captureEnd: endTime)))
                }
                continue
            }
            let length = units.reduce(0) { $0 + $1.text.leading.count + $1.text.original.count }
            if length > config.maxChars {
                out += abort(.tooLong)
            }
            return out
        }
    }

    private func firstMatch(of phrases: [[String]]) -> (first: Int, last: Int)? {
        var best: (first: Int, last: Int)?
        for phrase in phrases where phrase.count <= units.count {
            for i in 0...(units.count - phrase.count) {
                if let b = best, i >= b.first { break }
                if (0..<phrase.count).allSatisfy({ units[i + $0].text.norm == phrase[$0] }) {
                    best = (i, i + phrase.count - 1)
                    break
                }
            }
        }
        return best
    }

    static func clean(_ text: String) -> String {
        text.trimmingCharacters(in: CharacterSet.whitespacesAndNewlines.union(.punctuationCharacters))
    }
}
