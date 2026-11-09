package cn.sparknet.chat

import android.app.Activity
import android.content.Intent
import android.provider.DocumentsContract
import android.system.Os
import android.webkit.MimeTypeMap
import java.util.Locale
import android.net.Uri
import android.provider.OpenableColumns
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class DocumentArgs { lateinit var uri: String }

@TauriPlugin
class DocumentInfo(private val activity: Activity): Plugin(activity) {
  @Command
  fun beginPicker(invoke: Invoke) {
    try { FileTaskService.begin(activity); invoke.resolve(JSObject()) }
    catch (error: Exception) { invoke.reject(error.message ?: "无法保持文件操作期间的连接") }
  }
  @Command
  fun endPicker(invoke: Invoke) {
    FileTaskService.end(activity)
    invoke.resolve(JSObject())
  }
  @Command
  fun space(invoke: Invoke) {
    try {
      val uri = Uri.parse(invoke.parseArgs(DocumentArgs::class.java).uri)
      require(uri.scheme == "content")
      activity.contentResolver.openFileDescriptor(uri, "rw")!!.use { descriptor ->
        val stat = Os.fstatvfs(descriptor.fileDescriptor)
        invoke.resolve(JSObject().put("bytes", stat.f_bavail * stat.f_frsize))
      }
    } catch (error: Exception) { invoke.reject(error.message ?: "无法检查可用空间") }
  }
  @Command
  fun remove(invoke: Invoke) {
    try {
      val uri = Uri.parse(invoke.parseArgs(DocumentArgs::class.java).uri)
      require(uri.scheme == "content")
      require(DocumentsContract.deleteDocument(activity.contentResolver, uri))
      invoke.resolve(JSObject())
    } catch (error: Exception) { invoke.reject(error.message ?: "无法删除未完成文件") }
  }
  @Command
  fun openLink(invoke: Invoke) {
    try {
      val value = invoke.parseArgs(DocumentArgs::class.java).uri
      require(value.length <= 4096) { "链接过长" }
      val uri = Uri.parse(value)
      require(uri.scheme == "https" || uri.scheme == "http") { "只允许打开 HTTP 或 HTTPS 链接" }
      require(!uri.host.isNullOrBlank() && uri.userInfo == null) { "链接地址无效" }
      val intent = Intent(Intent.ACTION_VIEW, uri).addCategory(Intent.CATEGORY_BROWSABLE)
      activity.startActivity(intent)
      invoke.resolve(JSObject())
    } catch (error: Exception) { invoke.reject(error.message ?: "没有可打开链接的浏览器") }
  }
  @Command
  fun open(invoke: Invoke) {
    try {
      val uri = Uri.parse(invoke.parseArgs(DocumentArgs::class.java).uri)
      require(uri.scheme == "content")
      var mime = activity.contentResolver.getType(uri)
      if (mime == null || mime == "application/octet-stream" || mime == "*/*") {
        activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
          if (cursor.moveToFirst()) {
            val extension = cursor.getString(0)?.substringAfterLast('.', "")?.lowercase(Locale.ROOT)
            mime = extension?.let { MimeTypeMap.getSingleton().getMimeTypeFromExtension(it) }
          }
        }
      }
      val intent = Intent(Intent.ACTION_VIEW).setDataAndType(uri, mime ?: "application/octet-stream")
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      FileTaskService.begin(activity)
      activity.startActivity(Intent.createChooser(intent, "打开文件"))
      invoke.resolve(JSObject())
    } catch (error: Exception) { FileTaskService.end(activity); invoke.reject(error.message ?: "没有可打开此文件的应用") }
  }
  @Command
  fun metadata(invoke: Invoke) {
    try {
      val uri = Uri.parse(invoke.parseArgs(DocumentArgs::class.java).uri)
      require(uri.scheme == "content") { "Only system documents are supported" }
      var name = "文件"
      activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
        if (cursor.moveToFirst()) name = cursor.getString(0) ?: name
      }
      invoke.resolve(JSObject().put("name", name))
    } catch (error: Exception) {
      invoke.reject(error.message ?: "无法读取文件信息")
    }
  }
}
