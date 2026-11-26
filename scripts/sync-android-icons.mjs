#!/usr/bin/env node
/**
 * 同步安卓图标与原生宿主增强配置到 `tauri android init` 生成的 Android 工程。
 *
 * 为什么在此统一注入：
 *   `src-tauri/gen/` 不入库，CI 与新环境都会重新执行 `tauri android init`，
 *   而 Tauri 默认模板：
 *     1. 不会拷贝 `src-tauri/icons/android/` 自定义图标；
 *     2. Release 构建默认 `usesCleartextTraffic="false"`，会拦截本地自定义协议 `http://hg.localhost`；
 *     3. `MainActivity` 默认开启 `enableEdgeToEdge()` 但未向 WebView 注入状态栏/挖孔/导航栏安全区高度，
 *        也未开启 `mediaPlaybackRequiresUserGesture = false`、屏幕常亮及原生横竖屏/沉浸式系统栏切换。
 *
 *   `tauri.conf.json` 的 `beforeBuildCommand` 在 Gradle 编译前自动调用本脚本，确保本地和 CI 产物完全一致。
 */
import { cpSync, existsSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const androidRoot = join(root, "src-tauri", "gen", "android");
const iconSrc = join(root, "src-tauri", "icons", "android");
const resDest = join(androidRoot, "app", "src", "main", "res");

if (!existsSync(androidRoot)) {
  console.log("[android] 跳过：gen/android 尚未生成（先跑 tauri android init）");
  process.exit(0);
}

/* 1. 同步自定义安卓应用图标 */
if (existsSync(iconSrc) && existsSync(resDest)) {
  cpSync(iconSrc, resDest, { recursive: true, force: true });
  console.log(`[android] 安卓图标已同步 → ${resDest}`);
}

/* 2. 允许 Release 包访问本地自定义媒体代理协议 http://hg.localhost 与 http://tauri.localhost */
const appGradle = join(androidRoot, "app", "build.gradle.kts");
if (existsSync(appGradle)) {
  const raw = readFileSync(appGradle, "utf8");
  const next = raw.replace(
    'manifestPlaceholders["usesCleartextTraffic"] = "false"',
    'manifestPlaceholders["usesCleartextTraffic"] = "true"',
  );
  if (next !== raw) {
    writeFileSync(appGradle, next, "utf8");
    console.log("[android] 已开启 Release usesCleartextTraffic 支持本地媒体协议");
  }
}

/* 3. 加固 AndroidManifest.xml：硬件加速、画中画与屏幕旋转不重建 Activity */
const manifestPath = join(androidRoot, "app", "src", "main", "AndroidManifest.xml");
if (existsSync(manifestPath)) {
  let xml = readFileSync(manifestPath, "utf8");
  if (!xml.includes("android:hardwareAccelerated=")) {
    xml = xml.replace("<application", '<application\n        android:hardwareAccelerated="true"');
  }
  if (!xml.includes("android:supportsPictureInPicture=")) {
    xml = xml.replace(
      'android:name=".MainActivity"',
      'android:name=".MainActivity"\n            android:supportsPictureInPicture="true"',
    );
  }
  if (xml.includes("android:configChanges=") && !xml.includes("|density")) {
    xml = xml.replace(
      /android:configChanges="([^"]+)"/,
      'android:configChanges="$1|density"',
    );
  }
  writeFileSync(manifestPath, xml, "utf8");
  console.log("[android] 已更新 AndroidManifest.xml (硬件加速 / 画中画 / 旋转保持状态)");
}

/* 4. 配置 ProGuard / R8 保留 MainActivity 的 JNI 方法签名 */
const proguardPath = join(androidRoot, "app", "proguard-rules.pro");
if (existsSync(proguardPath)) {
  const raw = readFileSync(proguardPath, "utf8");
  const rule = "\n-keep class com.hghub.huangguo.MainActivity {\n    public *;\n}\n";
  if (!raw.includes("com.hghub.huangguo.MainActivity")) {
    writeFileSync(proguardPath, raw + rule, "utf8");
    console.log("[android] 已写入 R8 keep 规则保护 MainActivity JNI 方法");
  }
}

/* 5. 注入增强版 MainActivity.kt（原生横竖屏切换 + 沉浸系统栏 + Edge-to-Edge 安全区桥接 + 自动连播 + 常亮） */
function findFile(dir, targetName) {
  if (!existsSync(dir)) return null;
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    const st = statSync(full);
    if (st.isDirectory()) {
      const hit = findFile(full, targetName);
      if (hit) return hit;
    } else if (entry === targetName) {
      return full;
    }
  }
  return null;
}

const mainActivityPath = findFile(join(androidRoot, "app", "src", "main"), "MainActivity.kt");
if (mainActivityPath) {
  const kotlinSrc = `package com.hghub.huangguo

import android.content.pm.ActivityInfo
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.webkit.WebView
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

class MainActivity : TauriActivity() {
  private val uiHandler = Handler(Looper.getMainLooper())
  private var unlockOrientationRunnable: Runnable? = null
  private var inImmersiveLandscape = false
  @Volatile private var lastSatDp: Int = 0
  @Volatile private var lastSabDp: Int = 0
  private var cachedWebView: WebView? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge(
      statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
      navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT)
    )
    super.onCreate(savedInstanceState)
    window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    setupInsetsAndWebView()
  }

  private fun findWebView(root: View?): WebView? {
    if (root == null) return null
    if (root is WebView) return root
    if (root is ViewGroup) {
      for (i in 0 until root.childCount) {
        val found = findWebView(root.getChildAt(i))
        if (found != null) return found
      }
    }
    return null
  }

  private fun ensureWebViewConfigured(): WebView? {
    val existing = cachedWebView
    if (existing != null) return existing
    val wv = findWebView(window.decorView) ?: return null
    cachedWebView = wv
    wv.settings.mediaPlaybackRequiresUserGesture = false
    wv.setBackgroundColor(Color.parseColor("#1a1a1a"))
    wv.overScrollMode = View.OVER_SCROLL_NEVER
    pushInsetsToWebView(wv)
    return wv
  }

  private fun pushInsetsToWebView(wv: WebView) {
    val sat = lastSatDp
    val sab = lastSabDp
    val js = "document.documentElement.style.setProperty('--android-sat','\${sat}px');" +
      "document.documentElement.style.setProperty('--android-sab','\${sab}px');"
    wv.evaluateJavascript(js, null)
  }

  private fun setupInsetsAndWebView() {
    val decor = window.decorView
    ViewCompat.setOnApplyWindowInsetsListener(decor) { view, windowInsets ->
      val density = resources.displayMetrics.density.takeIf { it > 0f } ?: 1f
      val topBars = windowInsets.getInsets(
        WindowInsetsCompat.Type.statusBars() or WindowInsetsCompat.Type.displayCutout()
      )
      val bottomBars = windowInsets.getInsets(
        WindowInsetsCompat.Type.navigationBars()
      )
      lastSatDp = (topBars.top / density).toInt()
      lastSabDp = (bottomBars.bottom / density).toInt()
      ensureWebViewConfigured()?.let { pushInsetsToWebView(it) }
      ViewCompat.onApplyWindowInsets(view, windowInsets)
    }

    for (delay in longArrayOf(60L, 200L, 500L, 1200L, 2500L)) {
      uiHandler.postDelayed({
        ensureWebViewConfigured()?.let { pushInsetsToWebView(it) }
      }, delay)
    }
  }

  fun getSafeInsetsPacked(): Long {
    return ((lastSatDp.toLong() and 0xFFFFL) shl 16) or (lastSabDp.toLong() and 0xFFFFL)
  }

  fun applyStatusBarLight(light: Boolean) {
    uiHandler.post {
      val controller = WindowCompat.getInsetsController(window, window.decorView)
      controller.isAppearanceLightStatusBars = light
      controller.isAppearanceLightNavigationBars = light
    }
  }

  fun applyImmersiveOrientation(landscape: Boolean, immersive: Boolean) {
    uiHandler.post {
      unlockOrientationRunnable?.let { uiHandler.removeCallbacks(it) }
      unlockOrientationRunnable = null
      inImmersiveLandscape = landscape

      val controller = WindowCompat.getInsetsController(window, window.decorView)
      if (immersive || landscape) {
        controller.systemBarsBehavior =
          WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        controller.hide(WindowInsetsCompat.Type.systemBars())
      } else {
        controller.show(WindowInsetsCompat.Type.systemBars())
      }

      if (landscape) {
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
      } else {
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
        val restore = Runnable {
          if (!inImmersiveLandscape) {
            requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
          }
        }
        unlockOrientationRunnable = restore
        uiHandler.postDelayed(restore, 2200L)
      }
    }
  }
}
`;
  writeFileSync(mainActivityPath, kotlinSrc, "utf8");
  console.log(`[android] 已注入增强版 MainActivity.kt → ${mainActivityPath}`);
}
