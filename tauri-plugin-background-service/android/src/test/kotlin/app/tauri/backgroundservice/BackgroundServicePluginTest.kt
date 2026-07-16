package app.tauri.backgroundservice

import android.app.Activity
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.media.AudioManager
import android.os.Bundle
import android.telecom.CallAudioState
import android.telecom.Connection
import android.telecom.ConnectionRequest
import android.telecom.DisconnectCause
import android.telecom.TelecomManager
import androidx.test.core.app.ApplicationProvider
import app.tauri.plugin.Invoke
import com.fasterxml.jackson.databind.ObjectMapper
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * Unit tests for BackgroundServicePlugin SharedPreferences logic.
 *
 * Tests the actual SharedPreferences behavior that the @Command methods
 * rely on, rather than mocking SharedPreferences itself.
 *
 * Note: Full @Command method tests require Tauri Invoke objects which
 * need the Tauri Android framework. These tests verify the underlying
 * persistence logic.
 */
@RunWith(RobolectricTestRunner::class)
class BackgroundServicePluginTest {

    /** Concrete Activity for Robolectric's ActivityScenario. */
    class TestActivity : Activity()

    private lateinit var context: Context
    private lateinit var prefs: android.content.SharedPreferences

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)
        // Run CallActionReceiver's dispatch INLINE so its direct-invoke tests
        // (callActionReceiver_*) observe the post-dispatch state deterministically.
        // The default spawns a real worker (fire-and-forget) which would race the
        // assertions; the off-main test lives in Bgs20OffMainThreadTest and
        // installs its own thread-distinguishing executor (BGS-20, Step 11).
        CallActionReceiver.actionExecutor = { _, task -> task() }
    }

    @After
    fun tearDown() {
        // Restore the production ack-wait executor so per-test overrides don't leak.
        BackgroundServicePlugin.ackWaitExecutor =
            BackgroundServicePlugin.DEFAULT_ACK_WAIT_EXECUTOR
        // Restore the production call-action executor + dispatcher so per-test
        // overrides / the fake don't leak into other tests (Step 9 / Step 11).
        CallActionReceiver.actionExecutor = CallActionReceiver.DEFAULT_ACTION_EXECUTOR
        CallActionDispatch.dispatcher = RealCallActionDispatcher()
        // Drop any live Telecom connections registered by a Step-11 test so the
        // process-static registry doesn't leak across tests.
        BackgroundCallConnectionService.clearLiveConnectionsForTest()
    }

    // ── startKeepalive: persists label and service type ────────────────

    @Test
    fun startKeepalivePersistsLabelAndType() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        assertEquals("Syncing", prefs.getString("bg_service_label", null))
        assertEquals("dataSync", prefs.getString("bg_service_type", null))
    }

    @Test
    fun startKeepaliveWithSpecialUsePersistsType() {
        prefs.edit()
            .putString("bg_service_label", "Background Sync")
            .putString("bg_service_type", "specialUse")
            .apply()

        assertEquals("Background Sync", prefs.getString("bg_service_label", null))
        assertEquals("specialUse", prefs.getString("bg_service_type", null))
    }

    // ── stopKeepalive: clears all keys ──────────────────────────────────

    @Test
    fun stopKeepaliveClearsAllKeys() {
        // Set up initial state
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        DurableState.save(context, DurableState(
            recoveryPending = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        ))

        // Simulate stopKeepalive
        prefs.edit()
            .remove("bg_service_label")
            .remove("bg_service_type")
            .apply()
        DurableState.clear(context)

        assertNull(prefs.getString("bg_service_label", null))
        assertNull(prefs.getString("bg_service_type", null))
        assertFalse(DurableState.load(context).recoveryPending)
        assertEquals("", DurableState.load(context).lastServiceLabel)
    }

    // ── getAutoStartConfig: reads pending state ─────────────────────────

    @Test
    fun durableStateRecoveryPendingRoundtrip() {
        DurableState.save(context, DurableState(
            recoveryPending = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        ))

        val state = DurableState.load(context)
        assertTrue(state.recoveryPending)
        assertEquals("Syncing", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)
    }

    @Test
    fun durableStateDefaultsToNotPending() {
        DurableState.clear(context)

        assertFalse(DurableState.load(context).recoveryPending)
    }

    @Test
    fun durableStatePendingWithEmptyLabel() {
        DurableState.save(context, DurableState(recoveryPending = true))

        val state = DurableState.load(context)
        assertTrue(state.recoveryPending)
        assertEquals("", state.lastServiceLabel)
    }

    // ── clearAutoStartConfig: clears only recovery fields ───────────────

    @Test
    fun clearRecoveryFieldsPreservesOtherDurableState() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Active",
            lastServiceType = "dataSync",
            recoveryPending = true,
            recoveryReason = "os_restart",
        ))

        // Simulate clearing only recovery fields
        val current = DurableState.load(context)
        DurableState.save(context, current.copy(
            recoveryPending = false,
            recoveryReason = null,
        ))

        val state = DurableState.load(context)
        assertFalse(state.recoveryPending)
        assertNull(state.recoveryReason)

        // Other fields preserved
        assertTrue(state.desiredRunning)
        assertEquals("Active", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)
    }

    // ── load(): POST_NOTIFICATIONS permission request ──────────────────

    @Test
    @Config(sdk = [32]) // Below TIRAMISU (33) — no permission request
    fun loadDoesNotRequestPermissionsBelowApi33() {
        // On API < 33, POST_NOTIFICATIONS permission doesn't exist.
        // The load() method should skip the request entirely.
        // Verify by checking no permission request is pending.
        val activity = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        activity.onActivity { act ->
            val shadowActivity = shadowOf(act)
            // No permissions should have been requested
            assertNull(shadowActivity.lastRequestedPermission)
        }
    }

    @Test
    @Config(sdk = [33]) // TIRAMISU — should request permission if not granted
    fun loadRequestsPermissionsOnApi33WhenNotGranted() {
        val activity = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        activity.onActivity { act ->
            // Deny the permission first
            val shadowActivity = shadowOf(act)
            shadowActivity.denyPermissions(android.Manifest.permission.POST_NOTIFICATIONS)

            // After calling load(), the plugin would request the permission.
            // Since we can't construct the plugin without Tauri framework,
            // verify the permission check logic directly.
            assertFalse(
                act.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS)
                    == android.content.pm.PackageManager.PERMISSION_GRANTED
            )
        }
    }

    /**
     * spec-compliance W1 / R-W1.3: `startKeepalive` resolves POST_NOTIFICATIONS
     * BEFORE dispatching the service start intent (i.e. before the service's
     * first `startForeground`), so the persistent foreground notification is
     * allowed to post on Android 13+. The resolve is factored into
     * `ensureNotificationPermissionResolved`, which `startKeepalive` calls
     * up-front; here we drive it directly (a full `startKeepalive` needs the
     * Tauri Invoke + blocks on the start ACK).
     */
    @Test
    @Config(sdk = [33]) // TIRAMISU — POST_NOTIFICATIONS exists and must resolve
    fun post_notifications_resolved_before_first_startForeground() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val shadowActivity = shadowOf(act)
            shadowActivity.denyPermissions(android.Manifest.permission.POST_NOTIFICATIONS)

            val plugin = BackgroundServicePlugin(act)
            val alreadyGranted = plugin.ensureNotificationPermissionResolved()

            assertFalse(
                "permission was denied, so it is not already granted",
                alreadyGranted,
            )
            val request = shadowActivity.lastRequestedPermission
            assertNotNull("a permission request must be issued when denied", request)
            assertTrue(
                "POST_NOTIFICATIONS must be requested before the first startForeground",
                request!!.requestedPermissions.contains(
                    android.Manifest.permission.POST_NOTIFICATIONS
                ),
            )
        }
    }

    /**
     * BGS-21 (doc-08 Step 12 Task 2, Critic fix): `ensureNotificationPermissionResolved`
     * is the DEFAULT first-ask flow — driven from `load()` (gated by
     * `requestNotificationPermissionOnLoad`, which defaults true and is NOT
     * overridden in tauri.conf.json) AND `startKeepalive()`. It issues a real
     * `activity.requestPermissions(POST_NOTIFICATIONS)`, so it MUST persist
     * `hasAsked=true` exactly like the explicit `@Command requestNotificationPermission`
     * site — otherwise a user who denies this auto-prompt is mis-classified as
     * never-asked (`notDetermined`) instead of `denied`, and the auto-prompt
     * permanently-denied case regresses (`denied` -> `notDetermined`). Pins the
     * seam that the sibling `post_notifications_resolved_before_first_startForeground`
     * test leaves open (it asserts only the request, not the discriminator).
     */
    @Test
    @Config(sdk = [33]) // TIRAMISU — the POST_NOTIFICATIONS ask fires
    fun bgs21_ensureNotificationPermissionResolved_persistsHasAsked() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            // Start from a clean durable state so hasAsked is observably false.
            DurableState.clear(act)
            assertFalse(DurableState.load(act).hasAskedNotificationPermission)

            val shadowActivity = shadowOf(act)
            shadowActivity.denyPermissions(android.Manifest.permission.POST_NOTIFICATIONS)

            val plugin = BackgroundServicePlugin(act)
            val alreadyGranted = plugin.ensureNotificationPermissionResolved()

            // Denied -> the resolve issued an ask and reports not-already-granted.
            assertFalse(alreadyGranted)
            val request = shadowActivity.lastRequestedPermission
            assertNotNull("a permission request must be issued when denied", request)
            assertTrue(
                "POST_NOTIFICATIONS must be requested",
                request!!.requestedPermissions.contains(
                    android.Manifest.permission.POST_NOTIFICATIONS
                ),
            )
            // HEADLINE: the auto-prompt ask-site MUST persist hasAsked so a later
            // getNotificationPermissionStatus maps the denial to "denied".
            assertTrue(
                "ensureNotificationPermissionResolved must persist hasAsked=true after issuing the ask",
                DurableState.load(act).hasAskedNotificationPermission,
            )
        }
    }

    /**
     * BGS-22 (doc-08 Step 14): `requestBatteryExemption` fires the system
     * ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS intent for this app's package
     * so the user can grant the Doze exemption. The
     * REQUEST_IGNORE_BATTERY_OPTIMIZATIONS permission is declared in the plugin
     * AndroidManifest.xml but was never requested until this flow. The @Command
     * delegates to `launchBatteryExemptionRequest()` (a unit-testable seam)
     * because the full @Command needs a Tauri Invoke. NV-MUT: remove the
     * `activity.startActivity(intent)` inside `launchBatteryExemptionRequest`
     * ⇒ `nextStartedActivity` is null and only this assertion REDs.
     */
    @Test
    @Config(sdk = [33])
    fun bgs22_requestBatteryExemption_firesIgnoreBatteryOptimizationsIntent() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            plugin.launchBatteryExemptionRequest()

            val intent = shadowOf(act).nextStartedActivity
            assertNotNull(
                "requestBatteryExemption must fire a startActivity",
                intent,
            )
            assertEquals(
                "the intent must request the Doze-exemption dialog",
                android.provider.Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                intent!!.action,
            )
            assertEquals(
                "the intent data must target this app's package",
                "package:${act.packageName}",
                intent.dataString,
            )
        }
    }

    @Test
    @Config(sdk = [32]) // Below TIRAMISU — no POST_NOTIFICATIONS, resolve is a no-op grant
    fun post_notifications_resolve_isNoOpBelowApi33() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            assertTrue(
                "below API 33 the permission is implicitly granted",
                plugin.ensureNotificationPermissionResolved(),
            )
            assertNull(shadowOf(act).lastRequestedPermission)
        }
    }

    // ── NTF-09 (Step 10a): @PermissionCallback deferred-resolve ──────────────
    //
    // requestNotificationPermission defers resolution to onNotificationPermissionResult
    // (the @PermissionCallback) via Plugin.requestPermissionForAlias. The callback
    // re-queries checkSelfPermission — the ActivityResult carries no grantResults —
    // and resolves the deferred Invoke with {status: granted|denied} so the JS caller
    // observes the user's decision (not a fire-and-forget). The callback is driven
    // directly with a capturing Invoke (no Tauri PluginHandle needed). NV-MUT:
    // hardcode "granted" in the callback (skip the checkSelfPermission re-query) ->
    // the denied assertion REDs.

    @Test
    @Config(sdk = [33])
    fun onNotificationPermissionResult_granted_resolvesGrantedStatus() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            shadowOf(act).grantPermissions(android.Manifest.permission.POST_NOTIFICATIONS)
            val plugin = BackgroundServicePlugin(act)

            val resolved = AtomicReference<String?>(null)
            val invoke = Invoke(
                id = 1L,
                command = "requestNotificationPermission",
                callback = 100L,
                error = 101L,
                sendResponse = { _, data -> resolved.set(data) },
                argsJson = "{}",
                jsonMapper = ObjectMapper(),
            )
            plugin.onNotificationPermissionResult(invoke)

            val data = resolved.get()
            assertNotNull("the callback must resolve the deferred invoke", data)
            assertEquals(
                "granted permission resolves {status: granted}",
                "granted",
                org.json.JSONObject(data!!).getString("status"),
            )
        }
    }

    @Test
    @Config(sdk = [33])
    fun onNotificationPermissionResult_denied_resolvesDeniedStatus() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            shadowOf(act).denyPermissions(android.Manifest.permission.POST_NOTIFICATIONS)
            val plugin = BackgroundServicePlugin(act)

            val resolved = AtomicReference<String?>(null)
            val invoke = Invoke(
                id = 1L,
                command = "requestNotificationPermission",
                callback = 100L,
                error = 101L,
                sendResponse = { _, data -> resolved.set(data) },
                argsJson = "{}",
                jsonMapper = ObjectMapper(),
            )
            plugin.onNotificationPermissionResult(invoke)

            val data = resolved.get()
            assertNotNull("the callback must resolve the deferred invoke even when denied", data)
            assertEquals(
                "denied permission resolves {status: denied}",
                "denied",
                org.json.JSONObject(data!!).getString("status"),
            )
        }
    }

    @Test
    @Config(sdk = [32]) // Below TIRAMISU — POST_NOTIFICATIONS does not exist.
    fun onNotificationPermissionResult_belowApi33_resolvesGrantedStatus() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)

            val resolved = AtomicReference<String?>(null)
            val invoke = Invoke(
                id = 1L,
                command = "requestNotificationPermission",
                callback = 100L,
                error = 101L,
                sendResponse = { _, data -> resolved.set(data) },
                argsJson = "{}",
                jsonMapper = ObjectMapper(),
            )
            plugin.onNotificationPermissionResult(invoke)

            val data = resolved.get()
            assertNotNull(data)
            assertEquals(
                "below API 33 the permission is implicitly granted",
                "granted",
                org.json.JSONObject(data!!).getString("status"),
            )
        }
    }

    // ── load(): registers the self-managed Telecom phone account (C6 Step 15) ──

    /**
     * spec 08 C6 (Step 15): `load()` must register the self-managed
     * `PhoneAccount` so the OS can route audio focus, Bluetooth, and the
     * system call sheet through `BackgroundCallConnectionService` while the webview is
     * closed. Without this single registration call the whole
     * `BackgroundCallConnectionService` audio-focus / system-call-UI path is inert at
     * runtime — this is the regression guard for the init wiring.
     *
     * The plugin *can* be constructed directly: `Plugin`'s constructor only
     * stores the Activity and `Plugin.load` is a no-op default, so `load()`
     * runs under Robolectric exactly as it does on-device.
     */
    @Test
    @Config(sdk = [33]) // >= O (26) so registerPhoneAccount's API guard runs
    fun load_registersSelfManagedPhoneAccount() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            // load() never touches its webView arg (Plugin.load is a no-op),
            // so a Mockito stand-in keeps the assertion off the real WebView.
            plugin.load(org.mockito.Mockito.mock(android.webkit.WebView::class.java))

            val tm = act.getSystemService(Context.TELECOM_SERVICE) as TelecomManager
            val registeredHandles = shadowOf(tm).allPhoneAccounts
                .map { it.accountHandle }
            assertTrue(
                "load() must register the self-managed App phone account",
                registeredHandles.contains(BackgroundCallConnectionService.phoneAccountHandle(act))
            )
        }
    }

    // ── M-NATIVE-1 (Step 9): native Answer/Decline → Rust control plane ──
    //
    // The masked seam. `load_registersSelfManagedPhoneAccount` (above) is the
    // vacuous orphan-trap — it only asserts the PhoneAccount is *registered*,
    // never that an action reaches the core. These tests FIRE the Answer/Decline
    // broadcast and assert it dispatches `answer_call`/`reject_call` for the right
    // call_id (via an injected fake sink). NV-MUT (AC5): no-op the
    // `CallActionReceiver.onReceive` dispatch `when` block → these go RED while
    // `load_registersSelfManagedPhoneAccount` stays GREEN (the registration is
    // load-bearing-independent of the routing). Recorded in logs/step9-nvmut-*.log.

    @Test
    @Config(sdk = [34])
    fun callActionReceiver_answer_dispatchesAnswerCallAndCancelsRing() {
        val fake = FakeCallActionDispatcher()
        CallActionDispatch.dispatcher = fake

        // Post the ring so we can prove it is canceled on action.
        IncomingCallNotifier.showIncomingCall(
            context = context,
            callId = "call-ans",
            callerName = "Alice",
            isVideo = false,
            smallIcon = android.R.drawable.stat_notify_sync,
            launchIntent = null,
            useFullScreenIntent = true,
        )
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val notifId = IncomingCallNotifier.notificationIdFor("call-ans")
        assertNotNull("ring posted", shadowOf(nm).getNotification(notifId))

        val intent = Intent(CallActionReceiver.ACTION_CALL_ACTION).apply {
            putExtra(IncomingCallNotifier.EXTRA_CALL_ID, "call-ans")
            putExtra(IncomingCallNotifier.EXTRA_CALL_ACTION, IncomingCallNotifier.ACTION_ANSWER)
        }
        CallActionReceiver().onReceive(context, intent)

        assertEquals("Answer must reach core answer_call once", listOf("call-ans"), fake.answered)
        assertTrue("Answer must not reject", fake.rejected.isEmpty())
        assertNull("ring canceled on action", shadowOf(nm).getNotification(notifId))
    }

    @Test
    @Config(sdk = [34])
    fun callActionReceiver_decline_dispatchesRejectCall() {
        val fake = FakeCallActionDispatcher()
        CallActionDispatch.dispatcher = fake

        val intent = Intent(CallActionReceiver.ACTION_CALL_ACTION).apply {
            putExtra(IncomingCallNotifier.EXTRA_CALL_ID, "call-dec")
            putExtra(IncomingCallNotifier.EXTRA_CALL_ACTION, IncomingCallNotifier.ACTION_DECLINE)
        }
        CallActionReceiver().onReceive(context, intent)

        assertEquals("Decline must reach core reject_call once", listOf("call-dec"), fake.rejected)
        assertTrue("Decline must not answer", fake.answered.isEmpty())
    }

    @Test
    @Config(sdk = [34])
    fun callActionReceiver_unknownActionOrMissingExtras_dispatchNothing() {
        val fake = FakeCallActionDispatcher()
        CallActionDispatch.dispatcher = fake

        // Unknown action string → no dispatch.
        CallActionReceiver().onReceive(
            context,
            Intent(CallActionReceiver.ACTION_CALL_ACTION).apply {
                putExtra(IncomingCallNotifier.EXTRA_CALL_ID, "call-x")
                putExtra(IncomingCallNotifier.EXTRA_CALL_ACTION, "bogus")
            },
        )
        // Missing call_id → no dispatch (no crash).
        CallActionReceiver().onReceive(
            context,
            Intent(CallActionReceiver.ACTION_CALL_ACTION).apply {
                putExtra(IncomingCallNotifier.EXTRA_CALL_ACTION, IncomingCallNotifier.ACTION_ANSWER)
            },
        )

        assertTrue(fake.answered.isEmpty() && fake.rejected.isEmpty() && fake.ended.isEmpty())
    }

    @Test
    fun callIdFromRequest_readsCallIdExtra() {
        val handle = BackgroundCallConnectionService.phoneAccountHandle(context)
        val extras = Bundle().apply {
            putString(IncomingCallNotifier.EXTRA_CALL_ID, "tele-call-1")
        }
        val request = ConnectionRequest(handle, null, extras)
        assertEquals("tele-call-1", BackgroundCallConnectionService.callIdFromRequest(request))

        // Absent extra → empty (the broadcast route stays the primary binding).
        val bare = ConnectionRequest(handle, null, Bundle())
        assertEquals("", BackgroundCallConnectionService.callIdFromRequest(bare))
    }

    @Test
    @Config(sdk = [33]) // >= M for BackgroundCallConnection
    fun backgroundCallConnection_onAnswer_onReject_routeToCore() {
        val fake = FakeCallActionDispatcher()
        CallActionDispatch.dispatcher = fake

        val handle = BackgroundCallConnectionService.phoneAccountHandle(context)
        val extras = Bundle().apply {
            putString(IncomingCallNotifier.EXTRA_CALL_ID, "tele-call-2")
        }
        val request = ConnectionRequest(handle, null, extras)
        val connection = BackgroundCallConnectionService.BackgroundCallConnection(context, handle, request)
        assertEquals("connection binds the request call_id", "tele-call-2", connection.callId)

        connection.onAnswer()
        assertEquals(
            "Telecom onAnswer routes answer_call for this connection's call_id",
            listOf("tele-call-2"),
            fake.answered,
        )

        connection.onReject()
        assertEquals(
            "Telecom onReject routes reject_call for this connection's call_id",
            listOf("tele-call-2"),
            fake.rejected,
        )
    }

    // ── M-NATIVE-3 (Step 11): DRIVE the registered account + manage audio focus ──
    //
    // The masked seam. `load_registersSelfManagedPhoneAccount` only proves the
    // account is *registered*; these prove it is *DRIVEN* — an inbound offer issues
    // addNewIncomingCall, an active connection requests VOICE_COMMUNICATION focus +
    // MODE_IN_COMMUNICATION (abandoned on end), the Telecom-UI answer + the
    // notification-answer both drive the connection ACTIVE, and the route command
    // maps to a CallAudioState route. NV-MUT (AC5): stub addNewIncomingCall's body
    // → the issuance test REDs; stub requestCallAudioFocus → the focus tests RED;
    // registration/lifecycle neighbors stay GREEN. Recorded in logs/step11-nvmut-*.log.

    @Test
    @Config(sdk = [34])
    fun inboundOffer_issuesAddNewIncomingCallCarryingCallId() {
        BackgroundCallConnectionService.addNewIncomingCall(context, "tele-in-1", isVideo = false)

        val tm = context.getSystemService(Context.TELECOM_SERVICE) as TelecomManager
        val record = shadowOf(tm).onlyIncomingCall
        assertNotNull("an inbound offer must DRIVE the account via addNewIncomingCall", record)
        assertEquals(
            "addNewIncomingCall must target the App self-managed account",
            BackgroundCallConnectionService.phoneAccountHandle(context),
            record.phoneAccount,
        )
        assertEquals(
            "the call_id must ride the incoming-call extras (binds onAnswer/focus)",
            "tele-in-1",
            record.extras.getString(IncomingCallNotifier.EXTRA_CALL_ID),
        )
    }

    @Test
    @Config(sdk = [34])
    fun outboundDial_issuesPlaceCallCarryingCallId() {
        // The symmetric outbound capability (DEC-058): placeCall issues a
        // self-managed outgoing call carrying the call_id. (Its production wire to a
        // live dial event is a follow-on — there is no native outbound hook today;
        // outbound is core-initiated via start_call.)
        BackgroundCallConnectionService.placeOutgoingCall(context, "tele-out-1")

        val tm = context.getSystemService(Context.TELECOM_SERVICE) as TelecomManager
        val record = shadowOf(tm).onlyOutgoingCall
        assertNotNull("an outbound dial must issue placeCall", record)
        assertNotNull("placeCall must carry a routing address", record.address)
        assertEquals(
            "the call_id must ride the outgoing-call extras",
            "tele-out-1",
            record.extras.getString(IncomingCallNotifier.EXTRA_CALL_ID),
        )
    }

    @Test
    @Config(sdk = [33]) // >= M for BackgroundCallConnection + AudioFocusRequest
    fun activeConnection_requestsVoiceFocusAndCommunicationModeThenAbandons() {
        val handle = BackgroundCallConnectionService.phoneAccountHandle(context)
        val extras = Bundle().apply { putString(IncomingCallNotifier.EXTRA_CALL_ID, "focus-1") }
        val connection = BackgroundCallConnectionService.BackgroundCallConnection(
            context, handle, ConnectionRequest(handle, null, extras)
        )
        val am = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager

        // setActive() → onStateChanged(STATE_ACTIVE) → VoIP audio focus + IN_COMMUNICATION.
        connection.setActive()
        assertNotNull(
            "an active call must request VoIP audio focus",
            shadowOf(am).lastAudioFocusRequest,
        )
        assertEquals(
            "an active call must engage MODE_IN_COMMUNICATION",
            AudioManager.MODE_IN_COMMUNICATION,
            am.mode,
        )

        // setDisconnected → onStateChanged(STATE_DISCONNECTED) → abandon + MODE_NORMAL.
        connection.setDisconnected(DisconnectCause(DisconnectCause.LOCAL))
        assertNotNull(
            "ending a call must abandon the audio focus it requested",
            shadowOf(am).lastAbandonedAudioFocusRequest,
        )
        assertEquals(
            "ending a call must restore MODE_NORMAL",
            AudioManager.MODE_NORMAL,
            am.mode,
        )
    }

    @Test
    @Config(sdk = [33])
    fun onAnswer_routesCoreAndActivatesConnectionEngagingFocus() {
        val fake = FakeCallActionDispatcher()
        CallActionDispatch.dispatcher = fake
        val handle = BackgroundCallConnectionService.phoneAccountHandle(context)
        val extras = Bundle().apply { putString(IncomingCallNotifier.EXTRA_CALL_ID, "ans-active") }
        val connection = BackgroundCallConnectionService.BackgroundCallConnection(
            context, handle, ConnectionRequest(handle, null, extras)
        )
        val am = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager

        connection.onAnswer()

        // Step 9 preserved: answer still routes to the core.
        assertEquals("answer routes to core (Step 9)", listOf("ans-active"), fake.answered)
        // Step 11: the connection goes ACTIVE on answer, engaging audio focus.
        assertEquals("answer drives the connection ACTIVE", Connection.STATE_ACTIVE, connection.state)
        assertNotNull("answer engages VoIP audio focus", shadowOf(am).lastAudioFocusRequest)
        assertEquals(AudioManager.MODE_IN_COMMUNICATION, am.mode)
    }

    @Test
    @Config(sdk = [33])
    fun markCallActive_bridgesNotificationAnswerToConnectionFocus() {
        val handle = BackgroundCallConnectionService.phoneAccountHandle(context)
        val extras = Bundle().apply { putString(IncomingCallNotifier.EXTRA_CALL_ID, "notif-ans") }
        val connection = BackgroundCallConnectionService.BackgroundCallConnection(
            context, handle, ConnectionRequest(handle, null, extras)
        )
        BackgroundCallConnectionService.registerConnection("notif-ans", connection)
        val am = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager

        // The notification-answer bridge (CallActionReceiver answer → markCallActive).
        BackgroundCallConnectionService.markCallActive("notif-ans")
        assertEquals("markCallActive drives the live connection ACTIVE", Connection.STATE_ACTIVE, connection.state)
        assertNotNull("the bridged answer engages audio focus", shadowOf(am).lastAudioFocusRequest)
        assertEquals(AudioManager.MODE_IN_COMMUNICATION, am.mode)

        // Unknown call_id → no crash, nothing driven.
        BackgroundCallConnectionService.markCallActive("no-such-call")

        // Disconnect drops the registry entry (no leak).
        connection.setDisconnected(DisconnectCause(DisconnectCause.LOCAL))
        assertEquals("disconnect clears the live-connection registry", 0, BackgroundCallConnectionService.liveConnectionCount())
    }

    @Test
    fun audioRouteFor_mapsRouteStringsToCallAudioStateRoutes() {
        assertEquals("speaker → ROUTE_SPEAKER", CallAudioState.ROUTE_SPEAKER, BackgroundCallConnectionService.audioRouteFor("speaker"))
        assertEquals("earpiece → ROUTE_EARPIECE", CallAudioState.ROUTE_EARPIECE, BackgroundCallConnectionService.audioRouteFor("earpiece"))
        assertEquals("bluetooth → ROUTE_BLUETOOTH", CallAudioState.ROUTE_BLUETOOTH, BackgroundCallConnectionService.audioRouteFor("bluetooth"))
        assertNull("system is platform-managed (no override)", BackgroundCallConnectionService.audioRouteFor("system"))
        assertNull("unknown routes are ignored, not crashed", BackgroundCallConnectionService.audioRouteFor("bogus"))
    }

    @Test
    @Config(sdk = [33])
    fun setCallAudioRoute_appliesToLiveConnection_noopWhenAbsent() {
        // No live connection → no-op (no crash).
        BackgroundCallConnectionService.setCallAudioRoute("ghost", "speaker")

        val handle = BackgroundCallConnectionService.phoneAccountHandle(context)
        val extras = Bundle().apply { putString(IncomingCallNotifier.EXTRA_CALL_ID, "route-1") }
        val connection = BackgroundCallConnectionService.BackgroundCallConnection(
            context, handle, ConnectionRequest(handle, null, extras)
        )
        BackgroundCallConnectionService.registerConnection("route-1", connection)
        // Reaches the live connection (the physical route switch is device-verified;
        // here we prove the command resolves the route + reaches setAudioRoute without
        // throwing). `system` is a no-override no-op.
        BackgroundCallConnectionService.setCallAudioRoute("route-1", "speaker")
        BackgroundCallConnectionService.setCallAudioRoute("route-1", "system")
    }

    // ── Preflight FGS type validation ────────────────────────────────────

    @Test
    fun validateFgsTypeAllowedTypeReturnsNull() {
        val allowedTypes = listOf("dataSync", "specialUse")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "dataSync", allowedTypes, true
        )
        assertNull(result)
    }

    @Test
    fun validateFgsTypeUndeclaredTypeReturnsError() {
        val allowedTypes = listOf("dataSync")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", allowedTypes, true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        assertEquals("fgs_type_not_allowed", json.getString("code"))
        assertEquals("location", json.getString("invalidType"))
    }

    @Test
    fun validateFgsTypeSkippedWhenValidationDisabled() {
        val allowedTypes = listOf("dataSync")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", allowedTypes, false
        )
        assertNull(result)
    }

    @Test
    fun validateFgsTypeMultipleAllowedTypes() {
        val allowedTypes = listOf("dataSync", "location", "specialUse")
        assertNull(
            BackgroundServicePlugin.validateForegroundServiceType(
                "location", allowedTypes, true
            )
        )
        assertNull(
            BackgroundServicePlugin.validateForegroundServiceType(
                "specialUse", allowedTypes, true
            )
        )
        assertNotNull(
            BackgroundServicePlugin.validateForegroundServiceType(
                "mediaPlayback", allowedTypes, true
            )
        )
    }

    @Test
    fun validateFgsTypeEmptyAllowlistRejectsAll() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "dataSync", emptyList(), true
        )
        assertNotNull(result)
    }

    // ── Structured FGS validation error format ────────────────────────────

    @Test
    fun validateFgsType_structuredError_hasCodeField() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", listOf("dataSync"), true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        assertEquals("fgs_type_not_allowed", json.getString("code"))
    }

    @Test
    fun validateFgsType_structuredError_hasMessageField() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", listOf("dataSync"), true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        val message = json.getString("message")
        assertTrue("Message should mention the type", message.contains("location"))
        assertTrue("Message should mention config key", message.contains("androidForegroundServiceTypes"))
    }

    @Test
    fun validateFgsType_structuredError_hasInvalidTypeField() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "mediaPlayback", listOf("dataSync", "specialUse"), true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        assertEquals("mediaPlayback", json.getString("invalidType"))
    }

    @Test
    fun validateFgsType_structuredError_hasValidOptionsArray() {
        val allowed = listOf("dataSync", "specialUse", "location")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "camera", allowed, true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        val options = json.getJSONArray("validOptions")
        val actual = (0 until options.length()).map { options.getString(it) }
        assertEquals(allowed, actual)
    }

    // ── stopKeepalive clears DurableState ─────────────────────────────────

    @Test
    fun stopKeepaliveClearsDurableState() {
        // Simulate service was running with DurableState persisted
        val durableState = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 1000L,
        )
        DurableState.save(context, durableState)
        assertTrue("Precondition: DurableState should be saved",
            DurableState.load(context).desiredRunning)

        // Simulate stopKeepalive clearing DurableState
        DurableState.clear(context)

        val loaded = DurableState.load(context)
        assertFalse("desiredRunning should be false after clear", loaded.desiredRunning)
        assertEquals("", loaded.lastServiceLabel)
    }

    // ── computePermissionStatus ────────────────────────────────────────────

    @Test
    fun computePermissionStatus_granted_returnsGranted() {
        // granted short-circuits regardless of rationale / hasAsked
        assertEquals("granted",
            BackgroundServicePlugin.computePermissionStatus(true, false, false))
    }

    @Test
    fun computePermissionStatus_grantedWithRationale_returnsGranted() {
        // granted takes precedence over both rationale and hasAsked
        assertEquals("granted",
            BackgroundServicePlugin.computePermissionStatus(true, true, true))
    }

    @Test
    fun computePermissionStatus_notGranted_neverAsked_returnsNotDetermined() {
        // BGS-21: never asked (rationale suppressed by the system) -> notDetermined
        assertEquals("notDetermined",
            BackgroundServicePlugin.computePermissionStatus(false, false, false))
    }

    @Test
    fun computePermissionStatus_notGranted_permanentlyDenied_returnsDenied() {
        // BGS-21: asked + permanently blocked (rationale suppressed) -> denied
        assertEquals("denied",
            BackgroundServicePlugin.computePermissionStatus(false, false, true))
    }

    @Test
    fun bgs21_permission_status_correct() {
        // BGS-21 (doc-08 Step 12): Android `shouldShowRequestPermissionRationale`
        // is FALSE for BOTH never-asked AND permanently-denied, and TRUE only
        // after a first soft denial. It therefore CANNOT distinguish never-asked
        // from permanently-denied on its own — a persisted `hasAsked` flag is the
        // required discriminator. Verifies the full corrected mapping.
        // never-asked: not granted, never asked -> notDetermined (still promptable)
        assertEquals("notDetermined",
            BackgroundServicePlugin.computePermissionStatus(isGranted = false, shouldShowRationale = false, hasAsked = false))
        // permanently-denied ("don't ask again"): asked, rationale suppressed -> denied
        assertEquals("denied",
            BackgroundServicePlugin.computePermissionStatus(isGranted = false, shouldShowRationale = false, hasAsked = true))
        // denied-once (soft denial): asked, still re-askable -> denied
        assertEquals("denied",
            BackgroundServicePlugin.computePermissionStatus(isGranted = false, shouldShowRationale = true, hasAsked = true))
        // granted dominates regardless of the other signals
        assertEquals("granted",
            BackgroundServicePlugin.computePermissionStatus(isGranted = true, shouldShowRationale = false, hasAsked = false))
    }

    // ── loadConfig: requestNotificationPermissionOnLoad (NTF-09 Step 10a) ────
    //
    // Drives the production config parser (applyConfigJson) directly — NOT a bare
    // JSONObject.optBoolean probe — so each test pins the default the PLUGIN
    // actually applies. Tauri forwards the RAW tauri.conf.json plugin config to
    // mobile (plugin.rs raw_config -> mobile PluginManager.load); the app does
    // not set this key, so this optBoolean fallback IS the load-bearing default
    // for the load() startup prompt. NV-MUT: revert applyConfigJson's optBoolean
    // fallback to `true` -> defaultsToFalse REDs.

    @Test
    fun applyConfigJson_notificationPermissionOnLoad_defaultsToFalse() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            plugin.applyConfigJson("{}")
            assertFalse(
                "NTF-09: absent key defaults to false (no load() startup prompt)",
                plugin.requestNotificationPermissionOnLoad,
            )
        }
    }

    @Test
    fun applyConfigJson_notificationPermissionOnLoad_explicitTrueOverridesDefault() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            plugin.applyConfigJson("""{"androidRequestNotificationPermissionOnLoad":true}""")
            assertTrue(plugin.requestNotificationPermissionOnLoad)
        }
    }

    @Test
    fun applyConfigJson_notificationPermissionOnLoad_explicitFalseSticks() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            plugin.applyConfigJson("""{"androidRequestNotificationPermissionOnLoad":false}""")
            assertFalse(plugin.requestNotificationPermissionOnLoad)
        }
    }

    /**
     * NTF-09 (Step 10a) behavioral pin: with the default config (no key),
     * `load()` must NOT issue the unconditional POST_NOTIFICATIONS prompt — the
     * request is consented via the explainer (Step 10c). Direct construction
     * leaves `handle` null, so applyConfigJson early-returns and
     * `requestNotificationPermissionOnLoad` keeps its false initializer.
     * NV-MUT: revert the initializer to `true` -> load() prompts -> REDs.
     */
    @Test
    @Config(sdk = [33]) // TIRAMISU — POST_NOTIFICATIONS exists
    fun load_doesNotPromptForNotificationPermissionByDefault() {
        val scenario = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        scenario.onActivity { act ->
            val plugin = BackgroundServicePlugin(act)
            plugin.load(org.mockito.Mockito.mock(android.webkit.WebView::class.java))
            assertNull(
                "NTF-09: load() must not prompt for POST_NOTIFICATIONS by default",
                shadowOf(act).lastRequestedPermission,
            )
        }
    }

    // ── AC1: Prefs committed before start (uses commit, not apply) ─────────

    @Test
    fun startKeepalive_usesCommitInsteadOfApply() {
        // Simulates the fixed startKeepalive pref pattern:
        // commit() returns boolean, apply() returns Unit.
        // Verify that commit() is the call used for immediately-required prefs.
        val result = prefs.edit()
            .putString("bg_service_label", "App")
            .putString("bg_service_type", "remoteMessaging")
            .putString("bg_notif_channel_id", "bg_service")
            .putString("bg_notif_channel_name", "Background Service")
            .putInt("bg_notif_id", 9001)
            .putString("bg_notif_small_icon", null)
            .putBoolean("bg_show_stop_action", false)
            .putString("bg_on_timeout_policy", "notifyUser")
            .commit()

        assertTrue("commit() should return true on successful write", result)
        assertEquals("App", prefs.getString("bg_service_label", null))
        assertEquals("remoteMessaging", prefs.getString("bg_service_type", null))
    }

    @Test
    fun startKeepalive_prefsAreImmediatelyReadableAfterCommit() {
        // Verify that prefs written with commit() are immediately available
        // (unlike apply() which is async and may not be visible to onStartCommand)
        prefs.edit()
            .putString("bg_service_label", "App")
            .putString("bg_service_type", "remoteMessaging")
            .commit()

        // In production, LifecycleService.onStartCommand would read these prefs
        // immediately after startForegroundService returns.
        assertEquals("App", prefs.getString("bg_service_label", null))
        assertEquals("remoteMessaging", prefs.getString("bg_service_type", null))
    }

    // ── AC2: FGS_NOT_ALLOWED maps to structured error JSON ────────────────

    @Test
    fun mapServiceStartException_fgsNotAllowed_returnsStructuredError() {
        val exception = android.app.ForegroundServiceStartNotAllowedException("test")
        val result = BackgroundServicePlugin.mapServiceStartException(
            exception, "remoteMessaging"
        )
        val json = org.json.JSONObject(result as String)
        assertEquals("FGS_NOT_ALLOWED", json.getString("code"))
        assertTrue("Message should be non-empty",
            json.getString("message").isNotEmpty())
        assertEquals("remoteMessaging", json.getString("foregroundServiceType"))
    }

    // ── AC3: SECURITY maps to structured error JSON ───────────────────────

    @Test
    fun mapServiceStartException_securityException_returnsStructuredError() {
        val exception = SecurityException("Missing FOREGROUND_SERVICE permission")
        val result = BackgroundServicePlugin.mapServiceStartException(
            exception, "remoteMessaging"
        )
        val json = org.json.JSONObject(result as String)
        assertEquals("SECURITY", json.getString("code"))
        assertTrue("Message should contain exception message",
            json.getString("message").contains("FOREGROUND_SERVICE"))
        assertEquals("remoteMessaging", json.getString("foregroundServiceType"))
    }

    @Test
    fun mapServiceStartException_genericException_returnsUnknownError() {
        val exception = IllegalStateException("Service not ready")
        val result = BackgroundServicePlugin.mapServiceStartException(
            exception, "dataSync"
        )
        val json = org.json.JSONObject(result as String)
        assertEquals("UNKNOWN", json.getString("code"))
        assertTrue("Message should contain exception message",
            json.getString("message").contains("Service not ready"))
        assertEquals("dataSync", json.getString("foregroundServiceType"))
    }

    // ── AC4: Active prefs rolled back on start failure ────────────────────

    @Test
    fun rollbackActivePrefs_clearsServicePrefs() {
        // Simulate: prefs were committed before start, start failed, rollback needed
        prefs.edit()
            .putString("bg_service_label", "App")
            .putString("bg_service_type", "remoteMessaging")
            .putString("bg_notif_channel_id", "bg_service")
            .putString("bg_notif_channel_name", "Background Service")
            .putInt("bg_notif_id", 9001)
            .putString("bg_notif_small_icon", null)
            .putBoolean("bg_show_stop_action", false)
            .putString("bg_on_timeout_policy", "notifyUser")
            .commit()

        // Simulate rollback (the fix clears active prefs on failure)
        prefs.edit()
            .remove("bg_service_label")
            .remove("bg_service_type")
            .remove("bg_notif_channel_id")
            .remove("bg_notif_channel_name")
            .remove("bg_notif_id")
            .remove("bg_notif_small_icon")
            .remove("bg_show_stop_action")
            .remove("bg_on_timeout_policy")
            .commit()

        assertNull(prefs.getString("bg_service_label", null))
        assertNull(prefs.getString("bg_service_type", null))
        assertNull(prefs.getString("bg_notif_channel_id", null))
    }

    @Test
    fun rollbackActivePrefs_preservesDurableStateRecovery() {
        // Set up active service prefs and DurableState recovery
        prefs.edit()
            .putString("bg_service_label", "App")
            .putString("bg_service_type", "remoteMessaging")
            .commit()
        DurableState.save(context, DurableState(
            recoveryPending = true,
            lastServiceLabel = "Auto",
            lastServiceType = "dataSync",
        ))

        // Rollback only active service prefs
        prefs.edit()
            .remove("bg_service_label")
            .remove("bg_service_type")
            .commit()

        // DurableState should be preserved
        val state = DurableState.load(context)
        assertTrue(state.recoveryPending)
        assertEquals("Auto", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)

        // Service prefs should be cleared
        assertNull(prefs.getString("bg_service_label", null))
        assertNull(prefs.getString("bg_service_type", null))
    }

    @Test
    fun rollbackActivePrefs_persistsLastError() {
        // On failure, lastPlatformError should be persisted in DurableState
        val errorJson = BackgroundServicePlugin.mapServiceStartException(
            SecurityException("test"), "remoteMessaging"
        )
        assertNotNull("mapServiceStartException should return non-null", errorJson)
        val durableState = DurableState(
            desiredRunning = false,
            lastServiceLabel = "App",
            lastServiceType = "remoteMessaging",
            lastStartEpochMs = System.currentTimeMillis(),
            lastPlatformError = errorJson,
        )
        DurableState.save(context, durableState)

        val loaded = DurableState.load(context)
        assertNotNull("lastPlatformError should be persisted", loaded.lastPlatformError)
        assertTrue("Error should contain SECURITY",
            loaded.lastPlatformError!!.contains("SECURITY"))
    }

    // ── start-ACK wait must NOT block the caller (main looper) ─────────────
    // spec-compliance W1 / R-W1.2 deadlock regression: startKeepalive runs on the
    // main thread and the ACK is produced by LifecycleService.onStartCommand which
    // ALSO runs on the main thread. The on-device symptom of an inline (blocking)
    // wait was startForegroundCount=0 + a ~30 s FGS ANR. awaitStartAck must return
    // before a still-pending ack resolves, then resolve asynchronously once the
    // service completes the ack from its worker thread.

    /**
     * Block until the named ack-wait worker is parked inside `future.get()` (so
     * `await` has already grabbed the registry future) before the test completes
     * the ack — `ServiceStartAckRegistry.complete` removes the future from the
     * map, so completing before the worker grabs it would yield "ack_missing".
     * Production has no such race: `await` is well inside `get()` seconds before
     * `onStartCommand` completes the ack.
     */
    private fun awaitWorkerParked(name: String) {
        val deadline = System.currentTimeMillis() + 3000
        while (System.currentTimeMillis() < deadline) {
            val parked = Thread.getAllStackTraces().keys.any {
                it.name == name &&
                    (it.state == Thread.State.WAITING || it.state == Thread.State.TIMED_WAITING)
            }
            if (parked) return
            Thread.sleep(5)
        }
        fail("ack-wait worker '$name' never parked in get() — did awaitStartAck block the caller?")
    }

    private fun awaitTrue(flag: AtomicBoolean) {
        val deadline = System.currentTimeMillis() + 5000
        while (!flag.get() && System.currentTimeMillis() < deadline) Thread.sleep(10)
    }

    @Test
    fun awaitStartAck_doesNotBlockCaller_andResolvesAsyncOnSuccess() {
        BackgroundServicePlugin.ackWaitExecutor =
            BackgroundServicePlugin.DEFAULT_ACK_WAIT_EXECUTOR
        val id = "ack-async-success"
        ServiceStartAckRegistry.register(id)
        val resolved = AtomicBoolean(false)
        val rejected = AtomicBoolean(false)

        // Must return immediately even though the ack is still pending; an inline
        // (blocking) wait on the 30 s timeout would hang this test instead.
        BackgroundServicePlugin.awaitStartAck(
            id,
            onSuccess = { resolved.set(true) },
            onFailure = { rejected.set(true) },
        )
        assertFalse("ack wait must not resolve synchronously", resolved.get())

        // The service ("onStartCommand" worker) now completes the ack.
        awaitWorkerParked("bg-start-ack")
        ServiceStartAckRegistry.complete(id, true, "{\"ok\":true}")

        awaitTrue(resolved)
        assertTrue("onSuccess must run after the ack completes", resolved.get())
        assertFalse("onFailure must not run on success", rejected.get())
    }

    @Test
    fun awaitStartAck_rejectsAsyncWithPayloadOnFailure() {
        // Failure path: a non-blocking wait that, once the service reports a
        // failed start (success=false), invokes onFailure with the ack payload.
        BackgroundServicePlugin.ackWaitExecutor =
            BackgroundServicePlugin.DEFAULT_ACK_WAIT_EXECUTOR
        val id = "ack-async-failure"
        ServiceStartAckRegistry.register(id)
        val resolved = AtomicBoolean(false)
        val failed = AtomicBoolean(false)
        val failurePayload = AtomicReference<String?>(null)

        BackgroundServicePlugin.awaitStartAck(
            id,
            onSuccess = { resolved.set(true) },
            onFailure = { failurePayload.set(it.payload); failed.set(true) },
        )
        assertFalse("ack wait must not reject synchronously", failed.get())

        val payload = "{\"ok\":false,\"code\":\"core_start_failed\"}"
        awaitWorkerParked("bg-start-ack")
        ServiceStartAckRegistry.complete(id, false, payload)

        awaitTrue(failed)
        assertEquals("onFailure must receive the ack payload", payload, failurePayload.get())
        assertFalse("onSuccess must not run on failure", resolved.get())
    }
}
