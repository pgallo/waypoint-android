package io.github.pgallo.waypoint.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.pgallo.waypoint.core.LocalRelay
import io.github.pgallo.waypoint.core.LocationFix
import io.github.pgallo.waypoint.core.WaypointEngine
import io.github.pgallo.waypoint.data.WaypointRepository
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi

class MainViewModel(app: Application) : AndroidViewModel(app) {
    private val files = app.filesDir

    private val engine = WaypointEngine.open(files.resolve("waypoint.db").path, deviceId(app))
    private val peer = WaypointEngine.open(files.resolve("peer.db").path, "peer-tablet")
    private val relay = LocalRelay.open(files.resolve("relay.db").path)

    // One thread for all engine calls: the core serializes behind a mutex
    // anyway, and a single thread keeps the call order deterministic.
    @OptIn(ExperimentalCoroutinesApi::class)
    private val engineDispatcher = Dispatchers.IO.limitedParallelism(1)

    val repo = WaypointRepository(engine, peer, relay, viewModelScope, engineDispatcher)
    val state = repo.state

    init {
        repo.start()
    }

    fun onLocation(fix: LocationFix) = repo.onLocation(fix)

    override fun onCleared() {
        // Release native handles deterministically instead of waiting for the cleaner.
        engine.close()
        peer.close()
        relay.close()
    }

    private companion object {
        fun deviceId(app: Application): String {
            val prefs = app.getSharedPreferences("waypoint", 0)
            prefs.getString("node_id", null)?.let { return it }
            val id = "tablet-" + java.util.UUID.randomUUID().toString().take(8)
            prefs.edit().putString("node_id", id).apply()
            return id
        }
    }
}
