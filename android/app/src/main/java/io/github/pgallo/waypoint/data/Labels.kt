package io.github.pgallo.waypoint.data

import io.github.pgallo.waypoint.core.CheckInState
import io.github.pgallo.waypoint.core.FencePresence
import io.github.pgallo.waypoint.core.WorkStatus

// Presentation strings for core enums. Kept out of the composables so the
// mapping is exhaustive (`when` over sealed/enum types) and testable on the JVM.

fun WorkStatus.label(): String = when (this) {
    WorkStatus.OPEN -> "Open"
    WorkStatus.IN_PROGRESS -> "In progress"
    WorkStatus.PAUSED -> "Paused"
    WorkStatus.DONE -> "Done"
    WorkStatus.CANCELLED -> "Cancelled"
}

fun WorkStatus.isOpenForWork(): Boolean =
    this == WorkStatus.OPEN || this == WorkStatus.IN_PROGRESS || this == WorkStatus.PAUSED

fun FencePresence.label(): String = when (this) {
    FencePresence.UNKNOWN -> "No fix yet"
    FencePresence.INSIDE -> "On site"
    FencePresence.OUTSIDE -> "Off site"
}

fun CheckInState.label(): String = when (this) {
    CheckInState.Allowed -> "Inside the site geofence"
    CheckInState.NoLocationYet -> "Waiting for a reliable location fix"
    CheckInState.OutsideFence -> "Outside the site geofence"
    CheckInState.SiteHasNoFence -> "Site has no geofence; check-in needs dispatch"
    is CheckInState.AlreadyCheckedIn -> "Checked in by $by"
    CheckInState.Closed -> "Work order is closed"
}

fun CheckInState.canCheckIn(): Boolean = this == CheckInState.Allowed

fun CheckInState.isCheckedIn(): Boolean = this is CheckInState.AlreadyCheckedIn
