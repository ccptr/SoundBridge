package dev.ccptr.soundbridge

import android.app.Application

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        Bridge.init(this)
    }
}
