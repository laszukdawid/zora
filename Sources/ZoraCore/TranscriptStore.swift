import Foundation
import SQLite3

public enum StoreError: Error, CustomStringConvertible {
    case sqlite(String)
    public var description: String { if case .sqlite(let m) = self { return m }; return "store error" }
}

public struct StoredSegment: Sendable, Equatable {
    public var start: Date
    public var end: Date
    public var text: String
    public var kind: SegmentKind
}

public protocol TranscriptStoring: Sendable {
    func append(_ segment: TranscriptSegment, kind: SegmentKind) async
    func retagDictation(from start: Date, to end: Date) async
    func recordCommand(id: UUID, text: String, harness: String, status: String) async
    func finishCommand(id: UUID, status: String, exitCode: Int32?, summary: String) async
    func maintenance(retention: Double) async
    func flush() async
    func flushIfDue() async
    func takeFailure() async -> String?
}

/// SQLite transcript and command log with batched writes and rolling retention.
public actor SQLiteStore: TranscriptStoring {
    nonisolated(unsafe) private var db: OpaquePointer?
    private var pending: [(start: Double, end: Double, session: Int64, text: String, kind: String)] = []
    private var lastFlush = Date()
    private let batchSize = 32
    private let flushInterval: Double = 2

    /// `readOnly` opens an existing database for queries (e.g. `zora transcript`) without touching its state.
    public init(path: String, readOnly: Bool = false) throws {
        let dir = (path as NSString).deletingLastPathComponent
        if !readOnly {
            try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: dir)
        }
        var handle: OpaquePointer?
        let flags = readOnly ? SQLITE_OPEN_READONLY | SQLITE_OPEN_FULLMUTEX : SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX
        guard sqlite3_open_v2(path, &handle, flags, nil) == SQLITE_OK else {
            let msg = handle.map { String(cString: sqlite3_errmsg($0)) } ?? "open failed"
            sqlite3_close(handle)
            throw StoreError.sqlite(msg)
        }
        db = handle
        if readOnly { return }
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: path)
        for sql in [
            "PRAGMA journal_mode=WAL",
            "PRAGMA synchronous=NORMAL",
            "PRAGMA wal_autocheckpoint=256",
            "PRAGMA secure_delete=ON",
            """
            CREATE TABLE IF NOT EXISTS segments(
              id INTEGER PRIMARY KEY, session INTEGER NOT NULL, start_wall REAL NOT NULL, end_wall REAL NOT NULL,
              text TEXT NOT NULL, kind TEXT NOT NULL)
            """,
            "CREATE INDEX IF NOT EXISTS segments_end ON segments(end_wall)",
            """
            CREATE TABLE IF NOT EXISTS commands(
              id TEXT PRIMARY KEY, captured_wall REAL NOT NULL, text TEXT NOT NULL, harness TEXT NOT NULL,
              status TEXT NOT NULL, exit_code INTEGER, finished_wall REAL, summary TEXT)
            """,
            "CREATE INDEX IF NOT EXISTS commands_captured ON commands(captured_wall)",
        ] {
            try Self.exec(handle, sql)
        }
    }

    deinit { sqlite3_close(db) }

    /// Marks jobs cut short by a crash or restart; they are never replayed. Call once at service start.
    public func recoverInterruptedJobs() {
        run("UPDATE commands SET status='interrupted' WHERE status IN ('queued','running')", [])
    }

    public func append(_ segment: TranscriptSegment, kind: SegmentKind) {
        let now = HostClock.now(), wallNow = Date()
        pending.append((
            HostClock.wall(for: segment.start, now: now, wallNow: wallNow).timeIntervalSince1970,
            HostClock.wall(for: segment.end, now: now, wallNow: wallNow).timeIntervalSince1970,
            Int64(bitPattern: segment.session), segment.text, kind.rawValue
        ))
        if pending.count >= batchSize { flush() }
    }

    public func flush() {
        guard !pending.isEmpty else { return }
        let rows = pending
        pending.removeAll()
        lastFlush = Date()
        do {
            try Self.exec(db, "BEGIN")
            let stmt = try prepare("INSERT INTO segments(session,start_wall,end_wall,text,kind) VALUES(?,?,?,?,?)")
            defer { sqlite3_finalize(stmt) }
            for r in rows {
                sqlite3_reset(stmt)
                sqlite3_bind_int64(stmt, 1, r.session)
                sqlite3_bind_double(stmt, 2, r.start)
                sqlite3_bind_double(stmt, 3, r.end)
                sqlite3_bind_text(stmt, 4, r.text, -1, SQLITE_TRANSIENT)
                sqlite3_bind_text(stmt, 5, r.kind, -1, SQLITE_TRANSIENT)
                guard sqlite3_step(stmt) == SQLITE_DONE else { throw StoreError.sqlite(errmsg) }
            }
            try Self.exec(db, "COMMIT")
        } catch {
            try? Self.exec(db, "ROLLBACK")
            noteFailure("flush", error)
        }
    }

    public func retagDictation(from start: Date, to end: Date) {
        flush()
        run("UPDATE segments SET kind='dictation' WHERE end_wall >= ? AND start_wall <= ?", [start.timeIntervalSince1970, end.timeIntervalSince1970])
    }

    public func recordCommand(id: UUID, text: String, harness: String, status: String) {
        run("INSERT OR REPLACE INTO commands(id,captured_wall,text,harness,status) VALUES(?,?,?,?,?)",
            [id.uuidString, Date().timeIntervalSince1970, text, harness, status])
    }

    public func finishCommand(id: UUID, status: String, exitCode: Int32?, summary: String) {
        run("UPDATE commands SET status=?, exit_code=?, finished_wall=?, summary=? WHERE id=?",
            [status, exitCode.map { Int64($0) } as Any, Date().timeIntervalSince1970, summary, id.uuidString])
    }

    public func flushIfDue() {
        if Date().timeIntervalSince(lastFlush) >= flushInterval { flush() }
    }

    public func maintenance(retention: Double) {
        flush()
        let cutoff = Date().addingTimeInterval(-retention).timeIntervalSince1970
        run("DELETE FROM segments WHERE end_wall < ?", [cutoff])
        run("DELETE FROM commands WHERE captured_wall < ?", [cutoff])
        try? Self.exec(db, "PRAGMA wal_checkpoint(PASSIVE)")
    }

    public func segments(since: Date) -> [StoredSegment] {
        flush()
        guard let stmt = try? prepare("SELECT start_wall,end_wall,text,kind FROM segments WHERE end_wall >= ? ORDER BY start_wall") else { return [] }
        defer { sqlite3_finalize(stmt) }
        sqlite3_bind_double(stmt, 1, since.timeIntervalSince1970)
        var out: [StoredSegment] = []
        while sqlite3_step(stmt) == SQLITE_ROW {
            out.append(StoredSegment(
                start: Date(timeIntervalSince1970: sqlite3_column_double(stmt, 0)),
                end: Date(timeIntervalSince1970: sqlite3_column_double(stmt, 1)),
                text: String(cString: sqlite3_column_text(stmt, 2)),
                kind: SegmentKind(rawValue: String(cString: sqlite3_column_text(stmt, 3))) ?? .speech
            ))
        }
        return out
    }

    public func commands(since: Date) -> [(text: String, status: String, summary: String?)] {
        guard let stmt = try? prepare("SELECT text,status,summary FROM commands WHERE captured_wall >= ? ORDER BY captured_wall") else { return [] }
        defer { sqlite3_finalize(stmt) }
        sqlite3_bind_double(stmt, 1, since.timeIntervalSince1970)
        var out: [(String, String, String?)] = []
        while sqlite3_step(stmt) == SQLITE_ROW {
            out.append((String(cString: sqlite3_column_text(stmt, 0)), String(cString: sqlite3_column_text(stmt, 1)),
                        sqlite3_column_text(stmt, 2).map { String(cString: $0) }))
        }
        return out
    }

    public func count(_ table: String) -> Int {
        guard ["segments", "commands"].contains(table), let stmt = try? prepare("SELECT COUNT(*) FROM \(table)") else { return -1 }
        defer { sqlite3_finalize(stmt) }
        return sqlite3_step(stmt) == SQLITE_ROW ? Int(sqlite3_column_int64(stmt, 0)) : -1
    }

    // MARK: helpers

    private var errmsg: String { db.map { String(cString: sqlite3_errmsg($0)) } ?? "no db" }

    private func prepare(_ sql: String) throws -> OpaquePointer? {
        var stmt: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &stmt, nil) == SQLITE_OK else { throw StoreError.sqlite(errmsg) }
        return stmt
    }

    private func run(_ sql: String, _ params: [Any]) {
        do {
            let stmt = try prepare(sql)
            defer { sqlite3_finalize(stmt) }
            for (i, p) in params.enumerated() {
                let idx = Int32(i + 1)
                switch p {
                case let v as Double: sqlite3_bind_double(stmt, idx, v)
                case let v as Int64: sqlite3_bind_int64(stmt, idx, v)
                case let v as String: sqlite3_bind_text(stmt, idx, v, -1, SQLITE_TRANSIENT)
                default: sqlite3_bind_null(stmt, idx)
                }
            }
            guard sqlite3_step(stmt) == SQLITE_DONE else { throw StoreError.sqlite(errmsg) }
        } catch {
            noteFailure("statement", error)
        }
    }

    private static func exec(_ db: OpaquePointer?, _ sql: String) throws {
        var err: UnsafeMutablePointer<CChar>?
        guard sqlite3_exec(db, sql, nil, nil, &err) == SQLITE_OK else {
            let msg = err.map { String(cString: $0) } ?? "exec failed"
            sqlite3_free(err)
            throw StoreError.sqlite(msg)
        }
    }

    private var unreportedFailure: String?

    private func noteFailure(_ op: String, _ error: Error) {
        unreportedFailure = "transcript store \(op) failed: \(error)"
    }

    /// Returns (and clears) the last write failure so the caller can surface it.
    public func takeFailure() -> String? {
        defer { unreportedFailure = nil }
        return unreportedFailure
    }
}

private let SQLITE_TRANSIENT = unsafeBitCast(-1, to: sqlite3_destructor_type.self)
