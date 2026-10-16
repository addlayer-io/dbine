package io.addlayer.dbine.mobile

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters
import org.json.JSONArray
import java.util.concurrent.TimeUnit

// Android's periodic check (docs/mobile-companion.md): every 15 minutes,
// also with the app closed, asks DBine for news (long queries that ended,
// task runs) through the app's Rust code and shows them. The app's own loop
// does the same every 20 seconds while it runs; both keep their place in
// the same file, so a notice arrives once.
class NewsWorker(context: Context, params: WorkerParameters) : Worker(context, params) {
  companion object {
    private const val WORK = "dbine-news"
    private const val CHANNEL = "dbine-news"

    init {
      System.loadLibrary("dbine_mobile_lib")
    }

    /** The new notices as JSON (`[]`: none, or DBine out of reach). */
    @JvmStatic
    external fun check(dataDir: String): String

    /** Every 15 minutes (Android's minimum), only with a network. */
    fun schedule(context: Context) {
      val request = PeriodicWorkRequestBuilder<NewsWorker>(15, TimeUnit.MINUTES)
        .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
        .build()
      WorkManager.getInstance(context).enqueueUniquePeriodicWork(WORK, ExistingPeriodicWorkPolicy.KEEP, request)
    }
  }

  override fun doWork(): Result {
    val events = try {
      KeyVault.load(applicationContext)
      JSONArray(check(applicationContext.dataDir.absolutePath))
    } catch (e: Throwable) {
      return Result.success()
    }
    if (events.length() == 0) return Result.success()
    if (Build.VERSION.SDK_INT >= 33 &&
      ContextCompat.checkSelfPermission(applicationContext, android.Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
    ) {
      return Result.success()
    }
    val manager = applicationContext.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (manager.getNotificationChannel(CHANNEL) == null) {
      manager.createNotificationChannel(NotificationChannel(CHANNEL, "DBine", NotificationManager.IMPORTANCE_DEFAULT))
    }
    val shown = NotificationManagerCompat.from(applicationContext)
    for (i in 0 until events.length()) {
      val e = events.getJSONObject(i)
      val n = NotificationCompat.Builder(applicationContext, CHANNEL)
        .setSmallIcon(R.drawable.ic_notification)
        .setColor(Color.parseColor("#3794FF"))
        .setContentTitle(e.optString("title"))
        .setContentText(e.optString("body"))
        .setStyle(NotificationCompat.BigTextStyle().bigText(e.optString("body")))
        .setAutoCancel(true)
        .build()
      try {
        shown.notify(("dbine-" + e.optLong("seq") + e.optString("at")).hashCode(), n)
      } catch (_: SecurityException) {
      }
    }
    return Result.success()
  }
}
