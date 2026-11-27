package com.rc.android.update

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.Settings
import android.util.Log
import androidx.core.content.FileProvider
import com.rc.android.BuildConfig
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.Executors

/** One published APK, read from `latest-android.json`. */
data class UpdateManifest(
    val version: String,
    val code: Long,
    val sha256: String,
    val size: Long,
    val notes: String,
    val urls: List<String>,
)

sealed interface UpdatePhase {
    data object Idle : UpdatePhase
    data object Checking : UpdatePhase
    data object UpToDate : UpdatePhase
    data class Available(val m: UpdateManifest) : UpdatePhase
    data class Downloading(val m: UpdateManifest, val done: Long, val total: Long) : UpdatePhase
    data class Ready(val m: UpdateManifest, val file: File) : UpdatePhase
    data class Failed(val message: String, val m: UpdateManifest?) : UpdatePhase
}

/**
 * Checks the release manifest, downloads the APK, and hands it to the system installer.
 *
 * Integrity: the manifest is not signed, so the downloaded APK must (1) match the manifest
 * sha256 and (2) be signed with the same certificate as the running app. Android refuses an
 * update signed by anyone else anyway; checking first gives a clear message instead.
 */
class AppUpdater(context: Context) {
    private val ctx = context.applicationContext
    private val io = Executors.newSingleThreadExecutor { r -> Thread(r, "rc-update").apply { isDaemon = true } }
    private val dir = File(ctx.cacheDir, "update")

    private val _phase = MutableStateFlow<UpdatePhase>(UpdatePhase.Idle)
    val phase: StateFlow<UpdatePhase> = _phase.asStateFlow()

    val currentVersion: String = packageInfo(0)?.versionName ?: BuildConfig.VERSION_NAME
    private val currentCode: Long = packageInfo(0)?.longVersionCode ?: BuildConfig.VERSION_CODE.toLong()

    /** Only ever runs when the user taps 检查更新; the app never checks on its own. */
    fun check() {
        val busy = _phase.value
        if (busy is UpdatePhase.Checking || busy is UpdatePhase.Downloading) return
        _phase.value = UpdatePhase.Checking
        io.execute {
            cleanupOld()
            val m = fetchManifest()
            _phase.value = when {
                m == null -> UpdatePhase.Failed("无法获取版本信息，请检查网络", null)
                m.code > currentCode -> UpdatePhase.Available(m)
                else -> UpdatePhase.UpToDate
            }
        }
    }

    /** 立即更新: download (or reuse an already verified file), then open the system installer. */
    fun update() {
        val m = (_phase.value as? UpdatePhase.Available)?.m ?: return
        _phase.value = UpdatePhase.Downloading(m, 0L, m.size)
        io.execute {
            val next = try {
                UpdatePhase.Ready(m, readyFileFor(m) ?: downloadAndVerify(m))
            } catch (e: Exception) {
                Log.w(TAG, "update download failed", e)
                UpdatePhase.Failed(e.message ?: "下载失败", m)
            }
            _phase.value = next
            if (next is UpdatePhase.Ready) install()
        }
    }

    fun retry() {
        val m = (_phase.value as? UpdatePhase.Failed)?.m
        if (m != null) {
            _phase.value = UpdatePhase.Available(m)
            update()
        } else {
            check()
        }
    }

    @Volatile
    private var awaitingPermission = false

    /**
     * Opens the system installer. Android first needs "install unknown apps" for this app;
     * then the system settings page opens instead and the install continues on return.
     */
    fun install() {
        val p = _phase.value as? UpdatePhase.Ready ?: return
        if (!ctx.packageManager.canRequestPackageInstalls()) {
            awaitingPermission = true
            val intent = Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${ctx.packageName}"))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            runCatching { ctx.startActivity(intent) }
            return
        }
        awaitingPermission = false
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.update", p.file)
        val intent = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        runCatching { ctx.startActivity(intent) }.onFailure {
            _phase.value = UpdatePhase.Failed("无法打开系统安装程序", p.m)
        }
    }

    /** Called from onResume: continue an install that waited for the permission page. */
    fun onResume() {
        if (awaitingPermission && ctx.packageManager.canRequestPackageInstalls()) install()
    }

    private fun fetchManifest(): UpdateManifest? {
        for (url in BuildConfig.UPDATE_MANIFEST_URLS.split(',').map { it.trim() }.filter { it.isNotEmpty() }) {
            try {
                val text = open(url, 8_000).run {
                    try {
                        if (responseCode !in 200..299) throw IllegalStateException("HTTP $responseCode")
                        inputStream.bufferedReader().use { it.readText() }
                    } finally {
                        disconnect()
                    }
                }
                // Shared release manifest (`latest.json`): the PC reads the top level, the
                // phone reads its own `android` section.
                val root = JSONObject(text)
                val j = root.optJSONObject("android") ?: throw IllegalStateException("no android entry")
                val urls = j.getJSONArray("urls").let { a -> List(a.length()) { a.getString(it) } }
                    .filter { it.startsWith("https://") || it.startsWith("http://127.0.0.1") }
                if (urls.isEmpty()) throw IllegalStateException("no download url")
                return UpdateManifest(
                    version = j.optString("version").ifEmpty { root.getString("version") }.trim().removePrefix("v"),
                    code = j.getLong("code"),
                    sha256 = j.getString("sha256").lowercase(),
                    size = j.optLong("size", -1L),
                    notes = j.optString("notes").ifEmpty { root.optString("notes", "") },
                    urls = urls,
                )
            } catch (e: Exception) {
                Log.w(TAG, "manifest $url: ${e.message}")
            }
        }
        return null
    }

    private fun downloadAndVerify(m: UpdateManifest): File {
        dir.mkdirs()
        val part = File(dir, "download.part")
        var lastError: Exception? = null
        for (url in m.urls) {
            try {
                val digest = MessageDigest.getInstance("SHA-256")
                val conn = open(url, 15_000)
                try {
                    if (conn.responseCode !in 200..299) throw IllegalStateException("HTTP ${conn.responseCode}")
                    val total = m.size.takeIf { it > 0 } ?: conn.contentLengthLong
                    var got = 0L
                    var lastEmit = 0L
                    conn.inputStream.use { input ->
                        part.outputStream().use { out ->
                            val buf = ByteArray(64 * 1024)
                            while (true) {
                                val n = input.read(buf)
                                if (n < 0) break
                                out.write(buf, 0, n)
                                digest.update(buf, 0, n)
                                got += n
                                if (got - lastEmit > 256 * 1024) {
                                    lastEmit = got
                                    _phase.value = UpdatePhase.Downloading(m, got, total)
                                }
                            }
                        }
                    }
                } finally {
                    conn.disconnect()
                }
                val hex = digest.digest().joinToString("") { "%02x".format(it) }
                if (hex != m.sha256) throw IllegalStateException("安装包校验失败，请重试")
                checkSignedLikeMe(part)
                val apk = apkFile(m)
                apk.delete()
                if (!part.renameTo(apk)) throw IllegalStateException("保存安装包失败")
                return apk
            } catch (e: Exception) {
                lastError = e
                part.delete()
                Log.w(TAG, "download $url: ${e.message}")
            }
        }
        throw lastError ?: IllegalStateException("没有可用的下载地址")
    }

    private fun checkSignedLikeMe(apk: File) {
        val pm = ctx.packageManager
        val theirs = pm.getPackageArchiveInfo(apk.path, PackageManager.GET_SIGNING_CERTIFICATES)
            ?: throw IllegalStateException("安装包已损坏")
        if (theirs.packageName != ctx.packageName) throw IllegalStateException("安装包不是本应用")
        val mine = packageInfo(PackageManager.GET_SIGNING_CERTIFICATES)
            ?: throw IllegalStateException("读取本机签名失败")
        if (signers(theirs) != signers(mine) || signers(mine).isEmpty()) {
            throw IllegalStateException("安装包签名不一致，已拒绝安装")
        }
    }

    private fun signers(info: PackageInfo): Set<String> {
        val si = info.signingInfo ?: return emptySet()
        val certs = if (si.hasMultipleSigners()) si.apkContentsSigners else si.signingCertificateHistory?.takeLast(1)?.toTypedArray()
        val md = MessageDigest.getInstance("SHA-256")
        return certs.orEmpty().map { s -> md.digest(s.toByteArray()).joinToString("") { "%02x".format(it) } }.toSet()
    }

    private fun apkFile(m: UpdateManifest) = File(dir, "remote-control-${m.code}.apk")

    private fun readyFileFor(m: UpdateManifest): File? {
        val f = apkFile(m)
        if (!f.isFile) return null
        val digest = MessageDigest.getInstance("SHA-256")
        f.inputStream().use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                digest.update(buf, 0, n)
            }
        }
        val hex = digest.digest().joinToString("") { "%02x".format(it) }
        return f.takeIf { hex == m.sha256 }
    }

    /** Drop downloaded APKs that are already installed (or older). */
    private fun cleanupOld() {
        dir.listFiles()?.forEach { f ->
            val code = f.name.removePrefix("remote-control-").removeSuffix(".apk").toLongOrNull()
            if (f.name == "download.part" || (code != null && code <= currentCode)) f.delete()
        }
    }

    private fun open(url: String, timeoutMs: Int): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = timeoutMs
            readTimeout = timeoutMs
            instanceFollowRedirects = true
            setRequestProperty("User-Agent", "remote-control-android/$currentVersion")
            setRequestProperty("Cache-Control", "no-cache")
        }

    @Suppress("DEPRECATION")
    private fun packageInfo(flags: Int): PackageInfo? = runCatching {
        ctx.packageManager.getPackageInfo(ctx.packageName, flags)
    }.getOrNull()

    companion object {
        private const val TAG = "AppUpdater"
    }
}
