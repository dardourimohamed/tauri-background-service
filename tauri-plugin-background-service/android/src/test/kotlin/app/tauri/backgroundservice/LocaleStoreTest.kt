package app.tauri.backgroundservice

import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * BGS-19 (doc-08 Step 16 T2) — pure-JVM lock on the locale store READ path
 * ([LocaleStore.loadFromDir] / [LocaleStore.normalize]); no `org.json`, no
 * Robolectric (avoids the android.jar stub NPE under `isReturnDefaultValues`).
 * The `load(context)` integration (resolving `applicationInfo.dataDir`) is pinned
 * end-to-end in [Bgs19NotifierLocalizationTest].
 */
class LocaleStoreTest {
    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun loadFromDir_defaultsToEnglish_whenMissing() {
        assertEquals(LocaleStore.EN, LocaleStore.loadFromDir(tmp.newFolder("data")))
    }

    @Test
    fun loadFromDir_readsArabic_oneLine() {
        val dir = tmp.newFolder("data")
        File(dir, "locale.json").writeText("{\"locale\": \"ar\"}")
        assertEquals(LocaleStore.AR, LocaleStore.loadFromDir(dir))
    }

    @Test
    fun loadFromDir_readsFrench_prettyPrinted() {
        // Mirrors the Rust `serde_json::to_string_pretty` shape.
        val dir = tmp.newFolder("data")
        File(dir, "locale.json").writeText("{\n  \"locale\": \"fr\"\n}")
        assertEquals(LocaleStore.FR, LocaleStore.loadFromDir(dir))
    }

    @Test
    fun loadFromDir_unknownCode_defaultsToEnglish() {
        val dir = tmp.newFolder("data")
        File(dir, "locale.json").writeText("{\"locale\": \"xx-unknown\"}")
        assertEquals(LocaleStore.EN, LocaleStore.loadFromDir(dir))
    }

    @Test
    fun loadFromDir_corruptStore_defaultsToEnglish() {
        val dir = tmp.newFolder("data")
        File(dir, "locale.json").writeText("not json at all")
        assertEquals(LocaleStore.EN, LocaleStore.loadFromDir(dir))
    }

    @Test
    fun loadFromDir_missingLocaleKey_defaultsToEnglish() {
        val dir = tmp.newFolder("data")
        File(dir, "locale.json").writeText("{\"other\": \"value\"}")
        assertEquals(LocaleStore.EN, LocaleStore.loadFromDir(dir))
    }

    @Test
    fun normalize_isCaseAndWhitespaceTolerant() {
        assertEquals(LocaleStore.AR, LocaleStore.normalize("AR"))
        assertEquals(LocaleStore.AR, LocaleStore.normalize("  ar "))
        assertEquals(LocaleStore.FR, LocaleStore.normalize("FR"))
        assertEquals(LocaleStore.EN, LocaleStore.normalize("de"))
        assertEquals(LocaleStore.EN, LocaleStore.normalize(""))
    }
}
