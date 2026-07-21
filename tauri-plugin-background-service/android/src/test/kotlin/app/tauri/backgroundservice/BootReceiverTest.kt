package app.tauri.backgroundservice

import android.app.Application
import android.app.NotificationManager
import android.content.ComponentName
import android.content.Context
import android.content.ContextWrapper
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
class BootReceiverTest {

    private lateinit var context: Context
    private lateinit var app: Application

    @Before
    fun setup() {
        app = ApplicationProvider.getApplicationContext()
        context = app
        DurableState.clear(context)
    }

    // ── LOCKED_BOOT_COMPLETED: always ignored ──────────────────────────

    @Test
    @Config(sdk = [33])
    fun onReceive_lockedBootCompleted_doesNotStartService() {
        DurableState.save(context, DurableState(desiredRunning = true))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_LOCKED_BOOT_COMPLETED))

        assertNull(
            "No service should start for LOCKED_BOOT_COMPLETED",
            Shadows.shadowOf(app).peekNextStartedService()
        )
    }

    // ── BOOT_COMPLETED: ignores when not desired ──────────────────────

    @Test
    @Config(sdk = [33])
    fun onReceive_bootCompleted_notDesired_doesNotStartService() {
        DurableState.save(context, DurableState(desiredRunning = false))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_BOOT_COMPLETED))

        assertNull(
            "No service when not desired",
            Shadows.shadowOf(app).peekNextStartedService()
        )
    }

    // ── BOOT_COMPLETED: starts service when allowed (pre-35) ──────────

    @Test
    @Config(sdk = [33])
    fun onReceive_bootCompleted_desiredRunning_startsService() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        ))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_BOOT_COMPLETED))

        val started = Shadows.shadowOf(app).peekNextStartedService()
        assertNotNull("Service should start", started)
        assertEquals(LifecycleService::class.java.name, started.component?.className)
        assertEquals(LifecycleService.ACTION_START, started.action)
        assertEquals("Syncing", started.getStringExtra(LifecycleService.EXTRA_LABEL))
        assertEquals("dataSync", started.getStringExtra(LifecycleService.EXTRA_SERVICE_TYPE))
    }

    // ── BOOT_COMPLETED: specialUse starts even at SDK 34 (not blocked) ─

    @Test
    @Config(sdk = [34])
    fun onReceive_bootCompleted_specialUse_startsService() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "specialUse",
        ))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_BOOT_COMPLETED))

        val started = Shadows.shadowOf(app).peekNextStartedService()
        assertNotNull("specialUse should start at any API level", started)
        assertEquals(LifecycleService::class.java.name, started.component?.className)
    }

    // ── MY_PACKAGE_REPLACED: starts service (not subject to boot restrictions)

    @Test
    @Config(sdk = [33])
    fun onReceive_myPackageReplaced_desiredRunning_startsService() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        ))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_MY_PACKAGE_REPLACED))

        val started = Shadows.shadowOf(app).peekNextStartedService()
        assertNotNull("Service should start for MY_PACKAGE_REPLACED", started)
        assertEquals(LifecycleService::class.java.name, started.component?.className)
        assertEquals(LifecycleService.ACTION_START, started.action)
    }

    // ── AND-10: API-35 package-replaced path (direct delegation) ────────

    /**
     * AND-10: MY_PACKAGE_REPLACED is not subject to the API-35 boot-time FGS
     * type restrictions, so it must start the recovery service at API 35 too,
     * delegating label/type/reason through the recovery intent. Direct
     * unit-level coverage of a path the (Waydroid-gated) instrumentation
     * suite does not exercise at API 35.
     */
    @Test
    @Config(sdk = [35])
    fun onReceive_myPackageReplaced_api35_startsRecoveryServiceWithDelegatedArgs() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "remoteMessaging",
        ))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_MY_PACKAGE_REPLACED))

        val started = Shadows.shadowOf(app).peekNextStartedService()
        assertNotNull("MY_PACKAGE_REPLACED must start recovery at API 35", started)
        assertEquals(LifecycleService::class.java.name, started.component?.className)
        assertEquals(LifecycleService.ACTION_START, started.action)
        assertEquals(
            "delegated label rides the recovery intent",
            "Syncing",
            started.getStringExtra(LifecycleService.EXTRA_LABEL),
        )
        assertEquals(
            "delegated service type rides the recovery intent",
            "remoteMessaging",
            started.getStringExtra(LifecycleService.EXTRA_SERVICE_TYPE),
        )
        assertEquals(
            "delegated start reason is package_replaced",
            "package_replaced",
            started.getStringExtra(LifecycleService.EXTRA_START_REASON),
        )
    }

    // ── MY_PACKAGE_REPLACED: ignores when not desired ─────────────────

    @Test
    @Config(sdk = [33])
    fun onReceive_myPackageReplaced_notDesired_doesNotStartService() {
        DurableState.save(context, DurableState(desiredRunning = false))
        val receiver = BootReceiver()
        receiver.onReceive(context, Intent(Intent.ACTION_MY_PACKAGE_REPLACED))

        assertNull(
            "No service when not desired for MY_PACKAGE_REPLACED",
            Shadows.shadowOf(app).peekNextStartedService()
        )
    }

    // ── isBootBlockedType: pure companion tests ────────────────────────

    @Test
    fun isBootBlockedType_preApi35_notBlocked() {
        assertFalse(BootReceiver.isBootBlockedType("dataSync", 33))
        assertFalse(BootReceiver.isBootBlockedType("dataSync", 34))
    }

    @Test
    fun isBootBlockedType_api35_dataSync_blocked() {
        assertTrue(BootReceiver.isBootBlockedType("dataSync", 35))
        assertTrue(BootReceiver.isBootBlockedType("dataSync", 36))
    }

    @Test
    fun isBootBlockedType_api35_specialUse_notBlocked() {
        assertFalse(BootReceiver.isBootBlockedType("specialUse", 35))
    }

    @Test
    fun isBootBlockedType_api35_allBlockedTypes() {
        assertTrue(BootReceiver.isBootBlockedType("camera", 35))
        assertTrue(BootReceiver.isBootBlockedType("mediaPlayback", 35))
        assertTrue(BootReceiver.isBootBlockedType("phoneCall", 35))
        assertTrue(BootReceiver.isBootBlockedType("mediaProjection", 35))
        assertTrue(BootReceiver.isBootBlockedType("microphone", 35))
    }

    @Test
    fun isBootBlockedType_api35_allAllowedTypes() {
        assertFalse(BootReceiver.isBootBlockedType("connectedDevice", 35))
        assertFalse(BootReceiver.isBootBlockedType("location", 35))
        assertFalse(BootReceiver.isBootBlockedType("health", 35))
        assertFalse(BootReceiver.isBootBlockedType("remoteMessaging", 35))
        assertFalse(BootReceiver.isBootBlockedType("shortService", 35))
        assertFalse(BootReceiver.isBootBlockedType("systemExempted", 35))
        assertFalse(BootReceiver.isBootBlockedType("mediaProcessing", 35))
    }

    // ── postRecoveryNotification: creates channel and notification ─────

    @Test
    @Config(sdk = [33])
    fun postRecoveryNotification_createsChannelWithHighImportance() {
        BootReceiver.postRecoveryNotification(context, "Syncing")

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel(BootReceiver.RECOVERY_CHANNEL_ID)
        assertNotNull("Channel should be created", channel)
        assertEquals(BootReceiver.RECOVERY_CHANNEL_ID, channel.id)
        assertEquals(NotificationManager.IMPORTANCE_HIGH, channel.importance)
    }

    // ── Blocked-type recovery path: verify state persistence ───────────
    // Tests the side effects that handleBootCompleted produces when
    // isBootBlockedType returns true (API 35 receiver integration is
    // verified indirectly: isBootBlockedType + postRecoveryNotification).

    @Test
    @Config(sdk = [33])
    fun blockedTypeRecovery_persistsRecoveryPendingState() {
        val state = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        )
        // Simulate what handleBootCompleted does when blocked
        DurableState.save(context, state.copy(
            recoveryPending = true,
            recoveryReason = "boot_fgs_type_restricted",
        ))
        BootReceiver.postRecoveryNotification(context, state.lastServiceLabel)

        val saved = DurableState.load(context)
        assertTrue("recoveryPending should be true", saved.recoveryPending)
        assertEquals("boot_fgs_type_restricted", saved.recoveryReason)
        // desiredRunning remains true — service should resume when app opens
        assertTrue(saved.desiredRunning)
    }

    // ── AND-04: runtime FGS start rejection triggers durable recovery ────

    /**
     * AND-04: when the OS rejects the recovery start with a runtime
     * ForegroundServiceStartNotAllowedException-equivalent (NOT a type in the
     * static BOOT_BLOCKED_TYPES_API35 set), BootReceiver must persist recovery
     * and post one notification — the static set is only an optimization for the
     * known fast-path cases. Injects a context whose startForegroundService
     * throws to stand in for the device-only OS rejection.
     */
    @Test
    @Config(sdk = [35])
    fun bootCompleted_startRejectedAtRuntime_persistsRecoveryAndPostsNotification() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            // remoteMessaging is NOT in the static API-35 blocked set, so this
            // reaches startRecoveryService and exercises the runtime-reject path.
            lastServiceType = "remoteMessaging",
        ))
        val throwingContext = ThrowingStartContext(context)
        val receiver = BootReceiver()
        receiver.onReceive(throwingContext, Intent(Intent.ACTION_BOOT_COMPLETED))

        val state = DurableState.load(context)
        assertTrue(
            "a runtime-rejected start must persist recoveryPending (AND-04)",
            state.recoveryPending,
        )
        assertTrue(
            "recoveryReason must record the rejected start: ${state.recoveryReason}",
            state.recoveryReason?.startsWith("start_rejected:") == true,
        )
        // desiredRunning stays true so the user can resume when the app opens.
        assertTrue(state.desiredRunning)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val recoveryNotif = nm.activeNotifications.find { it.id == BootReceiver.RECOVERY_NOTIFICATION_ID }
        assertNotNull(
            "a runtime-rejected start must post exactly one recovery notification",
            recoveryNotif,
        )
    }

    /** A [ContextWrapper] whose foreground start always throws, simulating an OS start-restriction. */
    private class ThrowingStartContext(base: Context) : ContextWrapper(base) {
        override fun startForegroundService(service: Intent): ComponentName? =
            throw IllegalStateException("and04_test_fgs_start_blocked")
    }
}
