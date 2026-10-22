package io.github.pgallo.waypoint

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.core.content.ContextCompat
import io.github.pgallo.waypoint.location.LocationSource
import io.github.pgallo.waypoint.ui.MainViewModel
import io.github.pgallo.waypoint.ui.WaypointApp

class MainActivity : ComponentActivity() {
    private val vm: MainViewModel by viewModels()
    private lateinit var location: LocationSource

    private val permission =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { granted ->
            if (granted[Manifest.permission.ACCESS_FINE_LOCATION] == true) location.start()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        location = LocationSource(this, vm::onLocation)
        setContent { WaypointApp(vm.repo) }
    }

    override fun onStart() {
        super.onStart()
        val fine = ContextCompat.checkSelfPermission(this, Manifest.permission.ACCESS_FINE_LOCATION)
        if (fine == PackageManager.PERMISSION_GRANTED) {
            location.start()
        } else {
            permission.launch(
                arrayOf(Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION),
            )
        }
    }

    override fun onStop() {
        // Foreground-only tracking keeps the demo free of background-location
        // policy requirements; a production build would use a foreground service.
        location.stop()
        super.onStop()
    }
}
