package dev.velora.player

import android.app.NativeActivity
import android.content.pm.PackageManager
import android.os.Bundle

/** NativeActivity (Rust + Slint draw into it) with permission callbacks and lifecycle hooks. */
class VeloraActivity : NativeActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        MediaBridge.attach(this)
        super.onCreate(savedInstanceState) // loads libvelora.so (android.app.lib_name)
    }

    override fun onResume() {
        super.onResume()
        // app is visible again -> let the Rust side look for new files
        MediaBridge.sendCommand(MediaBridge.CMD_RESUME, 0)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == MediaBridge.REQ_AUDIO && grantResults.isNotEmpty() &&
            grantResults[0] == PackageManager.PERMISSION_GRANTED
        ) {
            MediaBridge.sendCommand(MediaBridge.CMD_PERMISSION_GRANTED, 0)
        }
    }
}
