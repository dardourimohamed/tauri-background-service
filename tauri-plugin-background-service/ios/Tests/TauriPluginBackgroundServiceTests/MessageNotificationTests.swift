import XCTest
import UserNotifications
@testable import tauri_plugin_background_service

/// IOS-MSG-01: the `showMessageNotification` native handler posts an actionable
/// `UNNotificationRequest` (stable identifier, metadata/deep-link userInfo,
/// reply + mark-read category) and resolves/rejects based on the scheduling
/// result rather than silently succeeding. Action routing flows through the
/// public `messageActionHandler` seam so the host's
/// `UNUserNotificationCenterDelegate` has a single testable entry point.
///
/// Before the fix, `MobileLifecycle::show_message_notification` returned `Ok(())`
/// on iOS without any Swift work — a promised operation that did nothing.
final class MessageNotificationTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var notificationCenter: FakeNotificationCenter!
    private var authorizer: FakeNotificationAuthorizer!
    private var suite: UserDefaults!

    private func args(
        notificationId: Int = 42,
        chatId: String = "chat-7",
        messageId: String = "msg-9",
        title: String = "Alice",
        body: String = "Hello",
        routeUri: String = "myapp://chat/chat-7/msg-9"
    ) -> String {
        return """
        {"notification_id":\(notificationId),"chat_id":\(chatId),\
        "message_id":\(messageId),"title":\(title),"body":\(body),\
        "route_uri":\(routeUri)}
        """
    }

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        notificationCenter = FakeNotificationCenter()
        authorizer = FakeNotificationAuthorizer()
        plugin.notificationCenter = notificationCenter
        plugin.notificationAuthorizer = authorizer
        // Keep the BGTaskScheduler seam no-op too so nothing reaches the real
        // scheduler if a path accidentally submits.
        plugin.scheduler = FakeBGTaskScheduler()
        suite = TestDefaults.makeIsolatedSuite()
        plugin.defaults = suite
        TestDefaults.clearAll(on: suite)
    }

    override func tearDown() {
        TestDefaults.clearAll(on: suite)
        BackgroundServicePlugin.messageActionHandler = nil
        plugin = nil
        notificationCenter = nil
        authorizer = nil
        suite = nil
        super.tearDown()
    }

    // MARK: - captured notification request

    func testShowMessageNotification_schedulesRequestWithStableIdentifier() {
        let capture = InvokeCapture()
        plugin.showMessageNotification(capture.makeInvoke(args: args()))

        XCTAssertEqual(capture.resolveCount, 1, "successful schedule resolves")
        XCTAssertEqual(notificationCenter.addCount, 1, "exactly one request was added")
        let request = notificationCenter.lastRequest
        XCTAssertEqual(request?.identifier, "message.chat-7.msg-9",
                       "identifier is derived from chat_id + message_id so a re-post replaces, not stacks")
    }

    func testShowMessageNotification_twoPostsForSameMessage_replaceNotStack() {
        plugin.showMessageNotification(InvokeCapture().makeInvoke(args: args()))
        plugin.showMessageNotification(InvokeCapture().makeInvoke(args: args()))

        XCTAssertEqual(notificationCenter.addCount, 2, "two posts each call add")
        // The stable identifier is the same for both — the system replaces the
        // prior notification. A different message would derive a different id.
        XCTAssertEqual(notificationCenter.lastRequest?.identifier, "message.chat-7.msg-9")
    }

    func testShowMessageNotification_distinctMessagesGetDistinctIdentifiers() {
        plugin.showMessageNotification(
            InvokeCapture().makeInvoke(args: args(messageId: "msg-1")))
        let firstId = notificationCenter.lastRequest?.identifier
        plugin.showMessageNotification(
            InvokeCapture().makeInvoke(args: args(messageId: "msg-2")))
        let secondId = notificationCenter.lastRequest?.identifier

        XCTAssertNotEqual(firstId, secondId, "distinct messages get distinct identifiers")
    }

    func testShowMessageNotification_userInfoCarriesRoutingAndDeepLink() {
        plugin.showMessageNotification(InvokeCapture().makeInvoke(args: args()))

        let userInfo = notificationCenter.lastRequest?.content.userInfo
        XCTAssertEqual(userInfo?["chat_id"] as? String, "chat-7",
                       "chat_id rides userInfo so the host can route tap/reply")
        XCTAssertEqual(userInfo?["message_id"] as? String, "msg-9",
                       "message_id rides userInfo so the host can route mark-read")
        XCTAssertEqual(userInfo?["notification_id"] as? Int, 42,
                       "notification_id rides userInfo for host-side cancellation")
        XCTAssertEqual(userInfo?["route_uri"] as? String, "myapp://chat/chat-7/msg-9",
                       "deep-link route_uri rides userInfo for tap-to-open")
        XCTAssertEqual(
            notificationCenter.lastRequest?.content.categoryIdentifier,
            BackgroundServicePlugin.messageNotificationCategoryId,
            "the category is set so the registered actions surface")
        XCTAssertEqual(
            notificationCenter.lastRequest?.content.title, "Alice")
        XCTAssertEqual(
            notificationCenter.lastRequest?.content.body, "Hello")
    }

    // MARK: - category registration with reply + mark-read

    func testShowMessageNotification_registersReplyAndMarkReadCategory() {
        plugin.showMessageNotification(InvokeCapture().makeInvoke(args: args()))

        XCTAssertEqual(notificationCenter.setCategoriesCount, 1,
                       "the actionable category is registered")
        let category = notificationCenter.lastCategories?.first
        XCTAssertEqual(category?.identifier, BackgroundServicePlugin.messageNotificationCategoryId)
        let actionIds = (category?.actions.map { $0.identifier }) ?? []
        XCTAssertEqual(actionIds.count, 2, "exactly reply + mark-read actions")
        XCTAssertTrue(actionIds.contains(BackgroundServicePlugin.messageReplyActionId),
                      "reply action is registered")
        XCTAssertTrue(actionIds.contains(BackgroundServicePlugin.messageMarkReadActionId),
                      "mark-read action is registered")
        // The reply action must be a text-input action so the user can type.
        let replyAction = category?.actions.first {
            $0.identifier == BackgroundServicePlugin.messageReplyActionId
        }
        XCTAssertTrue(replyAction is UNTextInputNotificationAction,
                      "reply action must accept typed text (UNTextInputNotificationAction)")
    }

    // MARK: - resolve / reject based on scheduling result (not silent success)

    func testShowMessageNotification_rejectsOnSchedulingError() {
        struct ScheduleError: Error {}
        notificationCenter.addError = ScheduleError()

        let capture = InvokeCapture()
        plugin.showMessageNotification(capture.makeInvoke(args: args()))

        XCTAssertEqual(capture.rejectCount, 1,
                       "scheduling failure rejects — never silently succeeds")
        XCTAssertEqual(capture.resolveCount, 0)
        XCTAssertTrue(capture.rejectedPayload?.contains("scheduleFailed") ?? false,
                      "rejection carries the scheduling failure: \(capture.rejectedPayload ?? "nil")")
    }

    func testShowMessageNotification_rejectsInvalidMessageIds() {
        // Empty chat_id — without routing keys the host can never deliver
        // tap/reply/mark-read back to the right conversation.
        let capture = InvokeCapture()
        plugin.showMessageNotification(
            capture.makeInvoke(args: args(chatId: "", messageId: "msg-1")))

        XCTAssertEqual(capture.rejectCount, 1)
        XCTAssertEqual(capture.resolveCount, 0)
        XCTAssertEqual(notificationCenter.addCount, 0,
                       "no request scheduled for invalid routing ids")
    }

    // MARK: - action routing through the public handler seam

    func testHandleMessageAction_routesReplyToPublicHandler_withTypedText() {
        var received: [(String, String, String, String?)] = []
        BackgroundServicePlugin.messageActionHandler = { action, chatId, messageId, reply in
            received.append((action, chatId, messageId, reply))
        }

        BackgroundServicePlugin.handleMessageAction(
            action: "reply", chatId: "chat-7", messageId: "msg-9", replyText: "Hi")

        XCTAssertEqual(received.count, 1)
        XCTAssertEqual(received.first?.0, "reply")
        XCTAssertEqual(received.first?.1, "chat-7")
        XCTAssertEqual(received.first?.2, "msg-9")
        XCTAssertEqual(received.first?.3, "Hi", "reply carries the typed text")
    }

    func testHandleMessageAction_routesMarkReadToPublicHandler_withNilText() {
        var received: [(String, String, String, String?)] = []
        BackgroundServicePlugin.messageActionHandler = { action, chatId, messageId, reply in
            received.append((action, chatId, messageId, reply))
        }

        BackgroundServicePlugin.handleMessageAction(
            action: "markRead", chatId: "chat-7", messageId: "msg-9", replyText: nil)

        XCTAssertEqual(received.count, 1)
        XCTAssertEqual(received.first?.0, "markRead")
        XCTAssertNil(received.first?.3, "markRead carries no reply text")
    }

    func testHandleMessageAction_logsMissingIntegration_whenHandlerIsNil() {
        // No handler wired — the route must still complete without crashing.
        BackgroundServicePlugin.messageActionHandler = nil
        BackgroundServicePlugin.handleMessageAction(
            action: "reply", chatId: "chat-7", messageId: "msg-9", replyText: "Hi")
        // No crash, no delivery — the os_log warning is the observable signal.
    }
}
