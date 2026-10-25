package cn.sparknet.chat

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import androidx.core.content.ContextCompat

// 系统选择器 / 查看器在前台时，避免缓存冻结切断聊天 TCP；不是后台常驻聊天。
class FileTaskService : Service() {
  private val timer = Handler(Looper.getMainLooper())
  override fun onBind(intent: Intent?): IBinder? = null
  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val manager = getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(NotificationChannel("file-task", "文件操作", NotificationManager.IMPORTANCE_LOW))
    val returnToChat = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    val notification = Notification.Builder(this, "file-task")
      .setSmallIcon(android.R.drawable.stat_sys_upload)
      .setContentTitle("Spark Chat · 文件操作")
      .setContentText("暂时保持聊天连接，返回聊天后自动停止")
      .setContentIntent(returnToChat).setOngoing(true).build()
    if (Build.VERSION.SDK_INT >= 29) startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    else startForeground(1, notification)
    timer.removeCallbacksAndMessages(null)
    timer.postDelayed({ stopSelf() }, 5 * 60 * 1000L)
    return START_NOT_STICKY
  }
  override fun onTimeout(startId: Int, fgsType: Int) { stopSelf() }
  override fun onDestroy() {
    timer.removeCallbacksAndMessages(null)
    stopForeground(STOP_FOREGROUND_REMOVE)
    super.onDestroy()
  }
  companion object {
    fun begin(context: Context) { ContextCompat.startForegroundService(context, Intent(context, FileTaskService::class.java)) }
    fun end(context: Context) { context.stopService(Intent(context, FileTaskService::class.java)) }
  }
}
