package io.github.pgallo.waypoint.location

import android.annotation.SuppressLint
import android.content.Context
import android.location.Location
import android.location.LocationListener
import android.location.LocationManager
import android.os.Bundle
import android.os.Looper
import io.github.pgallo.waypoint.core.LocationFix

/**
 * Platform GPS via the framework LocationManager (no proprietary location
 * SDK dependency). Raw fixes are forwarded untouched: filtering, outlier
 * rejection and geofence hysteresis all live in the Rust core so they behave
 * identically in tests, the CLI demo and on the device.
 */
class LocationSource(context: Context, private val onFix: (LocationFix) -> Unit) {
    private val manager = context.getSystemService(Context.LOCATION_SERVICE) as LocationManager

    // An object rather than a SAM lambda: before API 30 the provider and
    // status callbacks are abstract, and a lambda would throw
    // AbstractMethodError on API 26-29 when the framework calls them.
    private val listener = object : LocationListener {
        override fun onLocationChanged(loc: Location) {
            onFix(
                LocationFix(
                    lat = loc.latitude,
                    lon = loc.longitude,
                    timestampMs = loc.time,
                    accuracyM = if (loc.hasAccuracy()) loc.accuracy.toDouble() else UNKNOWN_ACCURACY_M,
                ),
            )
        }

        override fun onProviderEnabled(provider: String) = Unit

        override fun onProviderDisabled(provider: String) = Unit

        @Deprecated("Deprecated in API 29")
        override fun onStatusChanged(provider: String?, status: Int, extras: Bundle?) = Unit
    }

    private var running = false

    /** Caller must hold ACCESS_FINE_LOCATION. */
    @SuppressLint("MissingPermission")
    fun start() {
        if (running) return
        if (!manager.isProviderEnabled(LocationManager.GPS_PROVIDER)) return
        manager.requestLocationUpdates(
            LocationManager.GPS_PROVIDER,
            INTERVAL_MS,
            0f,
            listener,
            Looper.getMainLooper(),
        )
        running = true
    }

    fun stop() {
        if (!running) return
        manager.removeUpdates(listener)
        running = false
    }

    private companion object {
        const val INTERVAL_MS = 5_000L

        // Larger than any geofence band, so the core ignores such fixes.
        const val UNKNOWN_ACCURACY_M = 999.0
    }
}
