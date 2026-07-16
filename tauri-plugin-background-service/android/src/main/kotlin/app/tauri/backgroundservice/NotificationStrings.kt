package app.tauri.backgroundservice

/**
 * Localized notification-label string table (doc-08 / BGS-19 Step 16 T2).
 *
 * Mirrors the Rust `locale_store::locale_lookup` composition table for the
 * Kotlin-owned notification labels (message reply / mark-read action titles,
 * boot-recovery body, channel display names). The locale is resolved from the
 * Rust-persisted `locale.json` store via [LocaleStore]. Unknown keys and any ar/fr
 * gap fall back to the English master table — identical semantics to the Rust
 * side, so the two surfaces agree on every label.
 *
 * Templated keys carry a `{label}` placeholder the caller substitutes
 * (e.g. `.replace("{label}", label)`); under [LocaleStore.EN] the substituted
 * output is byte-identical to the pre-localization English literal, so existing
 * English assertions are preserved exactly.
 *
 * CROSS-DOC: doc 06 owns notification *rendering*; this is the i18n
 * composition/locale-plumbing seam (Kotlin half).
 */
object NotificationStrings {
    /** Resolve a localized notification label for [key] under [locale]. */
    fun lookup(key: String, locale: String): String = when (locale) {
        LocaleStore.AR -> ar(key) ?: en(key)
        LocaleStore.FR -> fr(key) ?: en(key)
        else -> en(key)
    }

    /** English master table — also the fallback for unknown keys and any ar/fr gap. */
    private fun en(key: String): String = when (key) {
        "reply" -> "Reply"
        "mark_as_read" -> "Mark as read"
        "tap_to_resume" -> "Tap to resume: {label}"
        "restarting" -> "Restarting..."
        "service_timed_out" -> "Background service timed out: {label}"
        "channel_messages" -> "Messages"
        "channel_messages_desc" -> "Message notifications"
        "channel_timeout" -> "Service Timeout"
        "channel_timeout_desc" -> "Notifications when background service times out"
        "channel_recovery" -> "Service Recovery"
        "channel_recovery_desc" -> "Notifications to resume background service after reboot"
        else -> ""
    }

    private fun ar(key: String): String? = when (key) {
        "reply" -> "رد"
        "mark_as_read" -> "ضع علامة كمقروء"
        "tap_to_resume" -> "اضغط للاستئناف: {label}"
        "restarting" -> "جارٍ إعادة التشغيل..."
        "service_timed_out" -> "انتهت مهلة خدمة الخلفية: {label}"
        "channel_messages" -> "الرسائل"
        "channel_messages_desc" -> "إشعارات الرسائل"
        "channel_timeout" -> "انتهاء مهلة الخدمة"
        "channel_timeout_desc" -> "إشعارات عند انتهاء مهلة خدمة الخلفية"
        "channel_recovery" -> "استعادة الخدمة"
        "channel_recovery_desc" -> "إشعارات لاستئناف خدمة الخلفية بعد إعادة التشغيل"
        else -> null
    }

    private fun fr(key: String): String? = when (key) {
        "reply" -> "Répondre"
        "mark_as_read" -> "Marquer comme lu"
        "tap_to_resume" -> "Appuyez pour reprendre : {label}"
        "restarting" -> "Redémarrage..."
        "service_timed_out" -> "Le service en arrière-plan a expiré : {label}"
        "channel_messages" -> "Messages"
        "channel_messages_desc" -> "Notifications de messages"
        "channel_timeout" -> "Expiration du service"
        "channel_timeout_desc" -> "Notifications lorsque le service en arrière-plan expire"
        "channel_recovery" -> "Récupération du service"
        "channel_recovery_desc" -> "Notifications pour reprendre le service en arrière-plan après le redémarrage"
        else -> null
    }
}
