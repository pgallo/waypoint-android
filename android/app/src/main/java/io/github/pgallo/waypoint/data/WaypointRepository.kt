package io.github.pgallo.waypoint.data

import io.github.pgallo.waypoint.core.CheckInState
import io.github.pgallo.waypoint.core.LocalRelay
import io.github.pgallo.waypoint.core.LocationFix
import io.github.pgallo.waypoint.core.SyncStatus
import io.github.pgallo.waypoint.core.TrackSummaryView
import io.github.pgallo.waypoint.core.WaypointEngine
import io.github.pgallo.waypoint.core.WaypointException
import io.github.pgallo.waypoint.core.WorkOrderView
import io.github.pgallo.waypoint.core.demoWalkFixes
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

data class SyncUi(
    val pending: Long = 0,
    val relayOps: Long = 0,
    val offline: Boolean = false,
    val last: SyncStatus? = null,
    val peerLast: SyncStatus? = null,
)

data class UiState(
    val nodeId: String = "",
    val orders: List<WorkOrderView> = emptyList(),
    val selected: WorkOrderView? = null,
    val checkIn: CheckInState? = null,
    val lastFix: LocationFix? = null,
    val fenceLog: List<String> = emptyList(),
    val recording: Boolean = false,
    val simulatingWalk: Boolean = false,
    val lastTrack: TrackSummaryView? = null,
    val sync: SyncUi = SyncUi(),
    val message: String? = null,
)

/**
 * Single owner of the Rust engine on the Kotlin side. Every call into the
 * core runs on [io] (the engine does disk I/O and holds a lock), and every
 * mutation ends with [refresh] so the UI only ever renders a snapshot that
 * the core produced. No domain rule is re-implemented here.
 *
 * [peer] is a second engine with its own database, standing in for another
 * crew member's tablet so the merge can be shown on a single device.
 */
class WaypointRepository(
    private val engine: WaypointEngine,
    private val peer: WaypointEngine,
    private val relay: LocalRelay,
    private val scope: CoroutineScope,
    private val io: CoroutineDispatcher,
    private val now: () -> Long = System::currentTimeMillis,
) {
    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    private var selectedId: String? = null
    private var walkJob: Job? = null

    fun start() = launchIo {
        if (engine.seedDemoData()) {
            // The seed plays the role of dispatch: publish it, then let the
            // simulated peer pick it up, as if both tablets were provisioned.
            engine.syncWith(relay)
            peer.syncWith(relay)
        }
        refresh()
    }

    fun select(id: String?) = launchIo {
        selectedId = id
        refresh()
    }

    fun checkIn() = mutateSelected { engine.checkIn(it) }

    fun checkOut(completed: Boolean) = mutateSelected { engine.checkOut(it, completed) }

    fun addNote(text: String) = mutateSelected { engine.addNote(it, text) }

    fun onLocation(fix: LocationFix) = launchIo { ingest(fix) }

    fun toggleRecording() = launchIo {
        if (engine.isRecording()) {
            val summary = engine.stopTrack()
            _state.update { it.copy(lastTrack = summary) }
        } else {
            engine.startTrack(selectedId)
        }
        refresh()
    }

    /** Replays the core's synthetic walk so the geofence can be exercised on an emulator. */
    fun toggleSimulatedWalk() {
        walkJob?.let {
            it.cancel()
            walkJob = null
            _state.update { s -> s.copy(simulatingWalk = false) }
            return
        }
        walkJob = scope.launch(io) {
            _state.update { it.copy(simulatingWalk = true) }
            try {
                for (fix in demoWalkFixes(now())) {
                    if (!isActive) break
                    guarded { ingest(fix) }
                    delay(WALK_STEP_MS)
                }
            } finally {
                _state.update { it.copy(simulatingWalk = false) }
                walkJob = null
            }
        }
    }

    fun setOffline(offline: Boolean) {
        _state.update { it.copy(sync = it.sync.copy(offline = offline)) }
    }

    fun syncNow() = launchIo {
        val status = if (_state.value.sync.offline) engine.syncOffline() else engine.syncWith(relay)
        _state.update { it.copy(sync = it.sync.copy(last = status)) }
        refresh()
    }

    /**
     * Makes a concurrent edit on the simulated peer and publishes it, so the
     * next local sync has something to merge.
     */
    fun peerEdit() = launchIo {
        peer.syncWith(relay)
        val target = peer.workOrders().firstOrNull { it.status.isOpenForWork() } ?: return@launchIo
        peer.addNote(target.id, "Peer: parts staged at gate (${target.siteName})")
        peer.setTitle(target.id, target.title.removeSuffix(" [peer]") + " [peer]")
        val status = peer.syncWith(relay)
        _state.update {
            it.copy(
                sync = it.sync.copy(peerLast = status),
                message = "Peer edited ${target.id} and synced",
            )
        }
        refresh()
    }

    fun dismissMessage() = _state.update { it.copy(message = null) }

    private suspend fun ingest(fix: LocationFix) {
        val events = engine.onLocation(fix)
        if (events.isNotEmpty()) {
            val lines = events.map { e ->
                (if (e.entered) "Entered " else "Left ") + e.siteId +
                    " (%+.0f m from boundary)".format(e.distanceM)
            }
            _state.update { it.copy(fenceLog = (lines + it.fenceLog).take(FENCE_LOG_SIZE)) }
        }
        _state.update { it.copy(lastFix = fix) }
        refresh()
    }

    private suspend fun refresh() = withContext(io) {
        val orders = engine.workOrders()
        val selected = selectedId?.let { id -> orders.firstOrNull { it.id == id } }
        val checkIn = selected?.let { engine.checkInState(it.id) }
        _state.update {
            it.copy(
                nodeId = engine.nodeId(),
                orders = orders,
                selected = selected,
                checkIn = checkIn,
                recording = engine.isRecording(),
                sync = it.sync.copy(
                    pending = engine.pendingOps().toLong(),
                    relayOps = relay.opCount().toLong(),
                ),
            )
        }
    }

    private fun mutateSelected(block: (String) -> Unit) = launchIo {
        val id = selectedId ?: return@launchIo
        block(id)
        refresh()
    }

    // Returns Unit on purpose: public actions are passed around as `() -> Unit`
    // UI callbacks, and the UI observes results through [state], not jobs.
    private fun launchIo(block: suspend () -> Unit) {
        scope.launch(io) { guarded(block) }
    }

    private suspend fun guarded(block: suspend () -> Unit) {
        try {
            block()
        } catch (e: WaypointException) {
            _state.update { it.copy(message = describe(e)) }
        }
    }

    companion object {
        const val WALK_STEP_MS = 120L
        const val FENCE_LOG_SIZE = 20

        fun describe(e: WaypointException): String = when (e) {
            is WaypointException.CheckInRefused -> "Check-in refused: ${e.reason.label()}"
            is WaypointException.Invalid -> e.detail
            is WaypointException.NotFound -> "Not found: ${e.detail}"
            is WaypointException.Storage -> "Storage error: ${e.detail}"
        }
    }
}
