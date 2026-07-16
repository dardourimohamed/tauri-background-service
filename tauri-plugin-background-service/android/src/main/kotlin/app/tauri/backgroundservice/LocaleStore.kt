package app.tauri.backgroundservice

import android.content.Context
import java.io.File

/**
 * Rust/Kotlin-readable UI-locale store — Kotlin read half (doc-08 / BGS-19 Step 16 T2).
 *
 * The UI locale is webview-only (`localStorage["lang"]`); the Rust `set_locale`
 * command mirrors it to `{app_data}/data/locale.json` (`{"locale":"ar"}`). This
 * object loads that SAME file so the Android notification composition path (which
 * runs without the webview — message action labels, boot-recovery text) honors the
 * user's chosen locale. Mirrors the Rust `locale_store::LocaleRecord::load`.
 *
 * The data dir matches `HeadlessBridge.dataDir`
 * (`applicationInfo.dataDir` + `"data"`), which is the same path Rust's
 * `BaseDirectory::AppData`/`"data"` resolves to on Android (one app, one UID, one
 * data dir) — so the Kotlin read sees exactly what the Rust write persists. A
 * missing/corrupt file or an unrecognized code falls back to `"en"`; headless
 * composition never fails on a bad store (same contract as the Rust side).
 *
 * Parsing intentionally AVOIDS `org.json`: the file shape is fixed
 * (`{"locale": <code>}`) and a substring scan keeps this object pure-JVM-testable
 * — the android.jar `org.json` stub nulls out under non-Robolectric unit tests
 * (`testOptions.unitTests.isReturnDefaultValues = true`), so a `JSONObject` parse
 * here would NPE in the pure test path (memory
 * `android-pure-jvm-test-orgjson-stub-npe-isreturndefaultvalues`).
 *
 * CROSS-DOC: doc 06 owns notification *rendering*; this is the i18n
 * composition/locale-plumbing seam (Kotlin half).
 */
object LocaleStore {
    private const val FILE_NAME = "locale.json"

    /** Supported locale codes persisted to/loaded from the store. */
    const val EN = "en"
    const val AR = "ar"
    const val FR = "fr"

    /** Load the persisted locale code for [context]'s data dir (default "en"). */
    fun load(context: Context): String = loadFromDir(dataDir(context))

    /**
     * Load the locale code from an explicit data dir (testable without a
     * `Context`). Missing file / parse error / unrecognized code ⇒ "en".
     */
    fun loadFromDir(dataDir: File): String {
        val text = runCatching { File(dataDir, FILE_NAME).readText() }.getOrNull()
            ?: return EN
        return normalize(extractLocaleCode(text))
    }

    /** Map a raw code to one of the supported codes (unrecognized ⇒ "en"). */
    fun normalize(code: String): String = when (code.trim().lowercase()) {
        AR -> AR
        FR -> FR
        else -> EN
    }

    private fun dataDir(context: Context): File {
        val base = context.applicationInfo?.dataDir
        return if (!base.isNullOrEmpty()) File(base, "data") else File(context.filesDir, "data")
    }

    /**
     * Extract the `locale` value from a `{"locale":"ar"}` payload WITHOUT
     * `org.json` (pure-JVM-safe). Tolerates the Rust `serde_json::to_string_pretty`
     * shape (`{\n  "locale": "ar"\n}`) and minor spacing variance.
     */
    private fun extractLocaleCode(text: String): String {
        val key = "\"locale\""
        val keyIdx = text.indexOf(key)
        if (keyIdx < 0) return ""
        val colonIdx = text.indexOf(':', keyIdx + key.length)
        if (colonIdx < 0) return ""
        val openQuote = text.indexOf('"', colonIdx + 1)
        if (openQuote < 0) return ""
        val closeQuote = text.indexOf('"', openQuote + 1)
        if (closeQuote < 0) return ""
        return text.substring(openQuote + 1, closeQuote)
    }
}
