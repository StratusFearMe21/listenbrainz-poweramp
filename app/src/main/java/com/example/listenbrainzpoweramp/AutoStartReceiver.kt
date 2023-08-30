package com.example.listenbrainzpoweramp

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

class AutoStartReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val serviceIntent = Intent(context, ForegroundService::class.java)
        context.startForegroundService(serviceIntent)
    }
}