package app.tauri.backgroundservice

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * BGS-19 (doc-08 Step 16 T2) — pure-JVM lock on the notification-label string
 * table. No Robolectric (the table is plain Kotlin; the ar/fr live-notification
 * behaviour is pinned in [Bgs19NotifierLocalizationTest]).
 */
class NotificationStringsTest {
    @Test
    fun lookup_repliesLocalized_perLocale() {
        assertEquals("Reply", NotificationStrings.lookup("reply", LocaleStore.EN))
        assertEquals("رد", NotificationStrings.lookup("reply", LocaleStore.AR))
        assertEquals("Répondre", NotificationStrings.lookup("reply", LocaleStore.FR))
    }

    @Test
    fun lookup_markReadLocalized_perLocale() {
        assertEquals("Mark as read", NotificationStrings.lookup("mark_as_read", LocaleStore.EN))
        assertEquals("ضع علامة كمقروء", NotificationStrings.lookup("mark_as_read", LocaleStore.AR))
        assertEquals("Marquer comme lu", NotificationStrings.lookup("mark_as_read", LocaleStore.FR))
    }

    @Test
    fun lookup_unknownLocaleFallsBackToEnglish() {
        // An unrecognized locale code ⇒ English master (never empty / never errors).
        assertEquals("Reply", NotificationStrings.lookup("reply", "de"))
        assertEquals("Reply", NotificationStrings.lookup("reply", ""))
    }

    @Test
    fun lookup_unknownKeyIsEmpty() {
        assertEquals("", NotificationStrings.lookup("does_not_exist", LocaleStore.AR))
    }

    @Test
    fun lookup_tapToResumeCarriesPlaceholder_underEachLocale() {
        // The {label} placeholder is present under every locale so the caller's
        // `.replace("{label}", label)` substitutes identically.
        listOf(LocaleStore.EN, LocaleStore.AR, LocaleStore.FR).forEach { loc ->
            val s = NotificationStrings.lookup("tap_to_resume", loc)
            assertTrue("tap_to_resume under $loc must carry {label}: $s", s.contains("{label}"))
        }
        // English composes byte-identically to the pre-localization literal once
        // the placeholder is substituted (existing English assertions preserved).
        assertEquals(
            "Tap to resume: Sila",
            NotificationStrings.lookup("tap_to_resume", LocaleStore.EN).replace("{label}", "Sila"),
        )
    }

    // ── BGS-19 Task A: LifecycleService FGS restart + timeout strings ───

    @Test
    fun lookup_restartingLocalized_perLocale() {
        assertEquals("Restarting...", NotificationStrings.lookup("restarting", LocaleStore.EN))
        assertEquals("جارٍ إعادة التشغيل...", NotificationStrings.lookup("restarting", LocaleStore.AR))
        assertEquals("Redémarrage...", NotificationStrings.lookup("restarting", LocaleStore.FR))
    }

    @Test
    fun lookup_channelTimeoutLocalized_perLocale() {
        // Channel display name + description. Android channels are immutable once
        // created so the name localizes only on first creation; these table values
        // are locked here (pure-JVM), the live BODY behavior in Bgs19LifecycleStringsTest.
        assertEquals("Service Timeout", NotificationStrings.lookup("channel_timeout", LocaleStore.EN))
        assertEquals("انتهاء مهلة الخدمة", NotificationStrings.lookup("channel_timeout", LocaleStore.AR))
        assertEquals("Expiration du service", NotificationStrings.lookup("channel_timeout", LocaleStore.FR))
    }

    @Test
    fun lookup_channelTimeoutDescLocalized_perLocale() {
        assertEquals(
            "Notifications when background service times out",
            NotificationStrings.lookup("channel_timeout_desc", LocaleStore.EN),
        )
        assertEquals(
            "إشعارات عند انتهاء مهلة خدمة الخلفية",
            NotificationStrings.lookup("channel_timeout_desc", LocaleStore.AR),
        )
        assertEquals(
            "Notifications lorsque le service en arrière-plan expire",
            NotificationStrings.lookup("channel_timeout_desc", LocaleStore.FR),
        )
    }

    @Test
    fun lookup_serviceTimedOutCarriesPlaceholder_andComposesByteIdentical_en() {
        // The {label} placeholder is present under every locale so the caller's
        // `.replace("{label}", label)` substitutes identically.
        listOf(LocaleStore.EN, LocaleStore.AR, LocaleStore.FR).forEach { loc ->
            val s = NotificationStrings.lookup("service_timed_out", loc)
            assertTrue("service_timed_out under $loc must carry {label}: $s", s.contains("{label}"))
        }
        // English composes byte-identically to the pre-localization literal
        // "Background service timed out: $label" once the placeholder is substituted.
        assertEquals(
            "Background service timed out: Syncing",
            NotificationStrings.lookup("service_timed_out", LocaleStore.EN).replace("{label}", "Syncing"),
        )
        // ar/fr render the substituted body the live notification posts.
        assertEquals(
            "انتهت مهلة خدمة الخلفية: Syncing",
            NotificationStrings.lookup("service_timed_out", LocaleStore.AR).replace("{label}", "Syncing"),
        )
        assertEquals(
            "Le service en arrière-plan a expiré : Syncing",
            NotificationStrings.lookup("service_timed_out", LocaleStore.FR).replace("{label}", "Syncing"),
        )
    }
}
