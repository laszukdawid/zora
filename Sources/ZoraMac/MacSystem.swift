import AVFAudio
import Foundation
import ServiceManagement
import UserNotifications
import ZoraCore
import os

private let log = Logger(subsystem: "dev.zora.agent", category: "system")

/// macOS notification banners. Works only when running from the signed Zora.app bundle.
public final class MacNotifier: NSObject, Notifier, UNUserNotificationCenterDelegate, @unchecked Sendable {
    public let id = "macos"
    private let center: UNUserNotificationCenter?
    private let warned = OSAllocatedUnfairLock(initialState: false)

    public override init() {
        // UNUserNotificationCenter traps outside an app bundle.
        center = Bundle.main.bundleIdentifier == nil ? nil : UNUserNotificationCenter.current()
        super.init()
        center?.delegate = self
    }

    public var isAvailable: Bool { center != nil }

    public func requestAuthorization() async -> Bool {
        guard let center else { return false }
        do { return try await center.requestAuthorization(options: [.alert, .sound]) } catch {
            log.error("notification authorization failed: \(String(describing: error), privacy: .public)")
            return false
        }
    }

    public func authorizationDescription() async -> String {
        guard let center else { return "unavailable (not running from Zora.app)" }
        switch await center.notificationSettings().authorizationStatus {
        case .authorized, .provisional, .ephemeral: return "authorized"
        case .denied: return "denied"
        case .notDetermined: return "not determined"
        @unknown default: return "unknown"
        }
    }

    public func notify(_ event: ZoraEvent) async {
        let (title, body) = event.display
        _ = await post(title: title, body: body)
    }

    /// Removes a delivered notification (e.g. "listening" after the user cancelled).
    public func withdraw(identifier: String) {
        center?.removeDeliveredNotifications(withIdentifiers: [identifier])
        center?.removePendingNotificationRequests(withIdentifiers: [identifier])
    }

    /// Returns whether the notification was handed to the system. A fixed `identifier` replaces the previous one.
    public func post(title: String, body: String, identifier: String = UUID().uuidString) async -> Bool {
        guard let center else { warnOnce("not running from Zora.app; notifications disabled"); return false }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = String(body.prefix(300))
        content.threadIdentifier = "zora"
        do {
            try await center.add(UNNotificationRequest(identifier: identifier, content: content, trigger: nil))
            return true
        } catch {
            warnOnce("notification delivery failed: \(error)")
            return false
        }
    }

    /// Clicking a notification needs no action; handling it here keeps macOS from launching another copy.
    public func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        completionHandler()
    }

    public func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .list])
    }

    private func warnOnce(_ message: String) {
        let first = warned.withLock { v in defer { v = true }; return !v }
        if first { log.error("\(message, privacy: .public)") }
    }
}

public enum Microphone {
    public static var status: String {
        switch AVAudioApplication.shared.recordPermission {
        case .granted: "granted"
        case .denied: "denied"
        case .undetermined: "not determined"
        @unknown default: "unknown"
        }
    }

    public static var granted: Bool { AVAudioApplication.shared.recordPermission == .granted }

    public static func request() async -> Bool {
        await withCheckedContinuation { cont in
            AVAudioApplication.requestRecordPermission { cont.resume(returning: $0) }
        }
    }
}

/// Launch-at-login through the LaunchAgent plist embedded in Zora.app.
public enum LoginItem {
    static let plistName = "dev.zora.agent.plist"
    static var service: SMAppService { SMAppService.agent(plistName: plistName) }

    public static func register() throws { try service.register() }
    public static func unregister() throws { try service.unregister() }

    public static var status: String {
        switch service.status {
        case .enabled: "enabled"
        case .notRegistered: "not registered"
        case .requiresApproval: "requires approval in System Settings → General → Login Items"
        case .notFound: "not found (run from Zora.app)"
        @unknown default: "unknown"
        }
    }
}
