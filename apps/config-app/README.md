# BerryKeep config app

## Managed client startup status

The config app shows **Starting** while a client service process is alive but
its launched local web port has not opened yet. Connection discovery can
take time when bootstrap endpoints are unreachable. Service cards refresh
automatically every two seconds without resetting unsaved configuration edits.
The web UI link becomes available once its port is listening; a process that
exits returns to **Stopped**, with its diagnostic log shown on the card.

Service actions disable their controls immediately. The backend also holds an
exclusive lock across process checks, identity preparation, and launch-report
updates, including across config-app processes using the same report path.
Overlapping actions return HTTP 409 with `code: "service_action_in_progress"`.
"Run Enabled Services" reuses already live processes, including clients still
starting, instead of replacing their recorded PID. Disabled services that are
still running remain tracked when enabled services are launched.

Configuration edits use a separate, short write lock, so slow service startup
does not block saving unrelated profiles. Configuration and launch-report
saves atomically replace the previous file, so readers see complete snapshots.
Bootstrap and client identity files use the same writer. Temporary Windows
sharing conflicts receive a short, bounded retry before an error is returned.
Recorded processes are reused or stopped only when their executable and
command arguments still match the launch report, preventing unrelated recycled
PIDs from being mistaken for services.

Identity updates reject concurrent changes with HTTP 409 and
`code: "client_identity_changed"`. If remote enrollment already completed,
the code is `client_identity_changed_after_enrollment`: no local credentials
are overwritten, but a single-use bootstrap or pairing token may have been
consumed, so obtain a new token before enrolling again.

If another service action prevents applying a saved identity to running
services, the response includes `reconciliation_deferred: true`. The UI warns
that the identity was saved and asks the user to restart affected services
after the current action finishes.
