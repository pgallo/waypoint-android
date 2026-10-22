package io.github.pgallo.waypoint.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.pgallo.waypoint.core.FencePresence
import io.github.pgallo.waypoint.core.WorkOrderView
import io.github.pgallo.waypoint.data.UiState
import io.github.pgallo.waypoint.data.WaypointRepository
import io.github.pgallo.waypoint.data.canCheckIn
import io.github.pgallo.waypoint.data.isCheckedIn
import io.github.pgallo.waypoint.data.label

private enum class Tab(val title: String) { Orders("Work orders"), Sync("Sync") }

@Composable
fun WaypointApp(repo: WaypointRepository) {
    val colors = if (isSystemInDarkTheme()) darkColorScheme() else lightColorScheme()
    MaterialTheme(colorScheme = colors) {
        val state by repo.state.collectAsStateWithLifecycle()
        var tab by rememberSaveable { mutableStateOf(Tab.Orders) }
        val snackbar = remember { SnackbarHostState() }

        LaunchedEffect(state.message) {
            state.message?.let {
                snackbar.showSnackbar(it)
                repo.dismissMessage()
            }
        }
        BackHandler(enabled = state.selected != null) { repo.select(null) }

        Scaffold(
            topBar = { WaypointTopBar(state, onBack = { repo.select(null) }) },
            snackbarHost = { SnackbarHost(snackbar) },
            bottomBar = {
                NavigationBar {
                    Tab.entries.forEach { t ->
                        NavigationBarItem(
                            selected = tab == t,
                            onClick = { tab = t },
                            icon = { Text(t.title.take(1), style = MaterialTheme.typography.titleMedium) },
                            label = { Text(if (t == Tab.Sync) "${t.title} (${state.sync.pending})" else t.title) },
                        )
                    }
                }
            },
        ) { padding ->
            val modifier = Modifier.padding(padding).fillMaxSize()
            when {
                tab == Tab.Sync -> SyncScreen(state, repo, modifier)
                state.selected != null -> ActiveJobScreen(state, state.selected!!, repo, modifier)
                else -> WorkOrdersScreen(state.orders, onOpen = repo::select, modifier = modifier)
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun WaypointTopBar(state: UiState, onBack: () -> Unit) {
    TopAppBar(
        title = {
            Column {
                Text(state.selected?.id ?: "Waypoint")
                Text(state.nodeId, style = MaterialTheme.typography.labelSmall)
            }
        },
        navigationIcon = {
            if (state.selected != null) TextButton(onClick = onBack) { Text("Back") }
        },
    )
}

// ----- Work orders list ------------------------------------------------------

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun WorkOrdersScreen(orders: List<WorkOrderView>, onOpen: (String) -> Unit, modifier: Modifier = Modifier) {
    LazyColumn(
        modifier = modifier,
        contentPadding = PaddingValues(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        if (orders.isEmpty()) {
            item { Text("No work orders yet.") }
        }
        items(orders, key = { it.id }) { wo ->
            Card(modifier = Modifier.fillMaxWidth().clickable { onOpen(wo.id) }) {
                Column(Modifier.padding(16.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("P${wo.priority}", style = MaterialTheme.typography.labelLarge)
                        Spacer(Modifier.width(8.dp))
                        Text(wo.title, style = MaterialTheme.typography.titleMedium)
                    }
                    Text("${wo.siteName}  ·  ${wo.crew}", style = MaterialTheme.typography.bodySmall)
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        AssistChip(onClick = { onOpen(wo.id) }, label = { Text(wo.status.label()) })
                        AssistChip(onClick = { onOpen(wo.id) }, label = { Text(wo.presence.label()) })
                        if (wo.notes.isNotEmpty()) {
                            AssistChip(onClick = { onOpen(wo.id) }, label = { Text("${wo.notes.size} notes") })
                        }
                    }
                }
            }
        }
    }
}

// ----- Active job ------------------------------------------------------------

@Composable
fun ActiveJobScreen(state: UiState, wo: WorkOrderView, repo: WaypointRepository, modifier: Modifier = Modifier) {
    var note by rememberSaveable(wo.id) { mutableStateOf("") }
    val checkIn = state.checkIn
    LazyColumn(
        modifier = modifier,
        contentPadding = PaddingValues(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        item {
            Text(wo.title, style = MaterialTheme.typography.headlineSmall)
            Text("${wo.siteName} · ${wo.status.label()} · priority ${wo.priority}")
        }
        item {
            Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Geofence: ${wo.presence.label()}", style = MaterialTheme.typography.titleSmall)
                    Text(checkIn?.label() ?: "")
                    state.lastFix?.let {
                        Text(
                            "Fix %.5f, %.5f ±%.0f m".format(it.lat, it.lon, it.accuracyM),
                            fontFamily = FontFamily.Monospace,
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        // The button is driven entirely by the core's CheckInState.
                        Button(onClick = repo::checkIn, enabled = checkIn?.canCheckIn() == true) {
                            Text("Check in")
                        }
                        if (checkIn?.isCheckedIn() == true) {
                            OutlinedButton(onClick = { repo.checkOut(completed = false) }) { Text("Pause") }
                            Button(onClick = { repo.checkOut(completed = true) }) { Text("Complete") }
                        }
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(onClick = repo::toggleSimulatedWalk) {
                            Text(if (state.simulatingWalk) "Stop simulated walk" else "Simulate walk to site")
                        }
                        OutlinedButton(onClick = repo::toggleRecording) {
                            Text(if (state.recording) "Stop track" else "Record track")
                        }
                    }
                    state.lastTrack?.let {
                        Text(
                            "Track: %d fixes -> %d points, %.0f m in %.0f s".format(
                                it.rawPoints.toInt(),
                                it.simplifiedPoints.toInt(),
                                it.distanceM,
                                it.durationS,
                            ),
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
            }
        }
        if (state.fenceLog.isNotEmpty()) {
            item { Text("Geofence events", style = MaterialTheme.typography.titleSmall) }
            items(state.fenceLog) { Text(it, style = MaterialTheme.typography.bodySmall) }
        }
        item { Text("Notes", style = MaterialTheme.typography.titleSmall) }
        items(wo.notes, key = { "${it.author}-${it.atMs}-${it.text.hashCode()}" }) { n ->
            Column {
                Text(n.text)
                Text(n.author, style = MaterialTheme.typography.labelSmall)
            }
        }
        item {
            Row(verticalAlignment = Alignment.CenterVertically) {
                OutlinedTextField(
                    value = note,
                    onValueChange = { note = it },
                    modifier = Modifier.weight(1f),
                    label = { Text("Add note") },
                )
                TextButton(
                    onClick = {
                        repo.addNote(note)
                        note = ""
                    },
                    enabled = note.isNotBlank(),
                ) { Text("Add") }
            }
        }
        if (wo.photos.isNotEmpty()) {
            item { Text("Photos (metadata)", style = MaterialTheme.typography.titleSmall) }
            items(wo.photos) { p ->
                Text("${p.sha256.take(10)}  ${p.width}x${p.height}  ${p.caption}", style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

// ----- Sync status -------------------------------------------------------------

@Composable
fun SyncScreen(state: UiState, repo: WaypointRepository, modifier: Modifier = Modifier) {
    val s = state.sync
    Column(
        modifier = modifier.padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text("Outbox: ${s.pending} pending ops", style = MaterialTheme.typography.titleMedium)
                Text("Relay log: ${s.relayOps} ops")
                s.last?.let {
                    Text("Last sync: pushed ${it.pushed}, pulled ${it.pulled}, applied ${it.applied}")
                    it.error?.let { e -> Text("Error: $e", color = MaterialTheme.colorScheme.error) }
                    Text("State ${it.fingerprint}", fontFamily = FontFamily.Monospace)
                }
                s.peerLast?.let {
                    Text("Peer state ${it.fingerprint}", fontFamily = FontFamily.Monospace)
                }
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Switch(checked = s.offline, onCheckedChange = repo::setOffline)
            Spacer(Modifier.width(8.dp))
            Text(if (s.offline) "Simulating no coverage" else "Relay reachable")
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = repo::syncNow) { Text("Sync now") }
            OutlinedButton(onClick = repo::peerEdit) { Text("Concurrent peer edit") }
        }
        Text(
            "Edits are written to the local SQLite outbox first and work without a connection. " +
                "Sync pushes the outbox, pulls new ops and merges them in the Rust core; " +
                "matching fingerprints mean both devices hold the same state.",
            style = MaterialTheme.typography.bodySmall,
        )
        val onSite = state.orders.count { it.presence == FencePresence.INSIDE }
        Text("Sites currently inside: $onSite", style = MaterialTheme.typography.bodySmall)
    }
}
