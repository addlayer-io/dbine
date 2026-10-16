package io.addlayer.dbine.mobile

import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.enableEdgeToEdge
import java.io.File

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // The pairing's file key, from the Keystore, before the app reads it.
    try {
      KeyVault.load(this)
    } catch (_: Exception) {
    }
    // News with the app closed (NewsWorker.kt).
    NewsWorker.schedule(this)
    // The phone's name, for pairing (the app's Rust reads it: device_name).
    try {
      File(dataDir, "device_name.txt").writeText(deviceName())
    } catch (_: Exception) {
    }
  }

  /** The name the user gave the phone, else its brand and model. */
  private fun deviceName(): String {
    // Each read on its own: newer Androids may refuse one (permissions).
    val named = listOf(
      runCatching { Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME) }.getOrNull(),
      runCatching { Settings.Secure.getString(contentResolver, "bluetooth_name") }.getOrNull(),
    ).firstOrNull { !it.isNullOrBlank() }
    if (named != null) return named.trim()
    val brand = Build.MANUFACTURER.replaceFirstChar { it.uppercase() }
    return if (Build.MODEL.startsWith(Build.MANUFACTURER, ignoreCase = true)) Build.MODEL else "$brand ${Build.MODEL}"
  }
}
