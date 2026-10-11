# Server-Admin Host Lifecycle Control Plan

## Status

Proposal only.

This document describes a safe, cross-platform way for BerryKeep Server Admin
to request either of these actions for the *local* node host:

- restart the BerryKeep Server Node service/runtime;
- restart the complete host/device.

It does not change the current Cockpit decision by itself. If this proposal is
accepted and implemented, it supersedes the relevant parts of
`docs/cockpit-host-administration-strategy.md`: Cockpit remains a supported,
independent administration option, but is no longer the only way to perform
these two actions.

## Problem and scope

Several Server Admin workflows already report that a restart is required. An
operator must currently leave BerryKeep and use the operating system's service
manager or Cockpit. That is inconvenient during a node replacement or swap
workflow, and it is inconsistent across packaged Ubuntu, macOS, Windows, and
Android nodes.

The desired common user experience is deliberately small:

| Requested action | Meaning |
| --- | --- |
| Restart BerryKeep node | Restart only the managed server-node process/runtime. |
| Restart host | Perform a coordinated operating-system or device reboot. |

In scope:

- a common capability, request, audit, and recovery model;
- packaged Ubuntu, macOS, and Windows Server Node deployments;
- the Android foreground-service deployment;
- a reusable Server Admin control that can be embedded in a future or existing
  node swap/replacement flow and in existing `restart required` notices.

Out of scope:

- arbitrary command execution, arbitrary systemd/launchd/Windows-service
  control, shutdown/power-off, package updates, disk operations, or remote
  shell access;
- a general replacement for Cockpit, MDM, or operating-system administration;
- bypassing Android's device-management restrictions;
- silently restarting a node as a side effect of saving configuration.

There is no dedicated Server Admin "Swap" page in the repository at the time
of writing. The UI should therefore be a reusable host-controls panel rather
than coupled to a presumed route.

## Current baseline

The current packages correctly keep the node itself unprivileged:

- Ubuntu/Debian uses the `berrykeep-server-node` systemd unit and the dedicated
  `berrykeep-server-node` account.
- macOS runs `io.berrykeep.server-node` as the `_berrykeep` user in a system
  `LaunchDaemon`.
- Windows runs `BerryKeepServerNode` as `NT AUTHORITY\LocalService`; its
  existing Service Control Manager handler already converts `STOP` into a
  graceful SDK shutdown signal.
- Android runs the embedded node in an application-owned foreground service.
  Its current stop path aborts the Rust task, so it needs a graceful lifecycle
  hand-off before it can provide a controlled restart.

Server Admin already has authenticated `/auth/...` routes and a local
administrator session. It currently deliberately tells operators to use a
separate Cockpit session for host administration. The new design must retain
the privilege boundary that motivated that choice.

## Design decision

Introduce a small **BerryKeep Host Control Agent** for desktop/server
platforms. It is a separately installed, privileged local process that accepts
only two fixed operations for the local BerryKeep installation. The
Server-Node process remains unprivileged and uses a local authenticated IPC
client to talk to it.

Android is the exception: a regular Android application has no equivalent
root-level host agent. Its application-owned foreground service is the
lifecycle controller for a node-runtime restart; a full device reboot is
available only when the app is provisioned as a Device Owner.

```text
authenticated Server Admin browser
        │ HTTPS + admin session + confirmation
        ▼
server-node SDK host-control API
        │ local, authenticated IPC; fixed request schema
        ▼
Host Control Agent / Android lifecycle controller
        │ native service-manager or device-management API
        ▼
BerryKeep service restart or host/device reboot
```

The server node must never receive root, `LocalSystem`, a broad sudo rule, or
the ability to interpolate an operator-supplied shell command.

### Why a local agent rather than direct OS calls from the node?

Giving the Server Node enough authority to restart itself or reboot the host
makes every server-side parsing, HTTP, and storage vulnerability a potential
host-administration vulnerability. A narrow, separately auditable agent keeps
the privileged surface to:

1. authenticate one local caller;
2. validate one of two enum values;
3. invoke one fixed, installation-owned service or reboot primitive;
4. record the result.

The agent must not listen on TCP, proxy Cockpit, accept shell text, accept a
unit/service name, or use `sudo`, PowerShell, `sh -c`, or `cmd.exe` to carry
out an action.

## Shared contract

Create a platform-neutral Rust protocol crate, for example
`crates/host-control-protocol`, plus an unprivileged client in
`server-node-sdk`. Keep OS-specific privilege and IPC code out of the SDK.

### Capabilities

The agent exposes a read-only capability document such as:

```json
{
  "protocol_version": 1,
  "platform": "ubuntu_systemd",
  "service_identity": "berrykeep-server-node.service",
  "enabled": true,
  "actions": {
    "restart_node": { "available": true },
    "reboot_host": { "available": false, "reason": "disabled by installer policy" }
  },
  "last_action": null
}
```

An unavailable capability is normal, not an error. Examples are an older
package without the agent, a deliberately disabled host reboot policy, or a
non-managed Android device.

### Requests and state

The only request actions are `restart_node` and `reboot_host`. A request
contains a server-generated UUID idempotency key, a one-time confirmation
challenge, a bounded grace period, and an audit correlation ID. It does not
contain a command, path, service name, executable, arguments, environment, or
force flag.

The agent persists a minimal action journal outside the node process, with
states such as:

```text
prepared → accepted → executing → requested
                             └→ rejected / failed
```

`requested` means the platform accepted the action, not that the reboot has
finished. This distinction is important because the HTTP connection and the
node that created it will normally disappear before completion.

The node exposes only admin-protected HTTP endpoints, for example:

```text
GET  /api/v1/auth/host-control/capabilities
POST /api/v1/auth/host-control/actions/prepare
POST /api/v1/auth/host-control/actions/{id}/confirm
GET  /api/v1/auth/host-control/actions/{id}
```

The `prepare` response is short-lived and binds the selected action, target
identity, preflight snapshot, and one-time nonce. `confirm` consumes it once.
This makes accidental form resubmission and stale browser tabs harmless.

### Reuse the existing admin authorization and audit trail

Host-control endpoints must compose the existing
`authorize_admin_request`/`append_admin_audit` mechanism rather than create
a second, parallel BerryKeep admin audit system.

The existing mechanism establishes the baseline admin authorization through an
admin-token header or an admin-session cookie, supports the generic
`dry_run`/ `approve` convention, and records actor/action metadata. The
host-control design should use it as follows:

- `prepare` calls it as a dry run, then appends an
  `auth/host-control/prepare` audit event with the target, preflight digest,
  and a newly generated correlation ID;
- `confirm` calls it as a non-dry-run approved action, validates the
  host-control challenge and browser-specific security gates below, records
  `auth/host-control/confirm`, and only then sends the IPC request;
- the privileged agent's durable journal records machine-level execution and
  refers to that correlation ID. It complements rather than replaces the
  existing admin audit trail.

The existing generic `approve` flag is necessary but not sufficient for a
host reboot. The short-lived, one-time `prepare`/ `confirm` challenge binds
that existing approval convention to a specific action and target.

### Authentication and authorization

There are two independent gates. The first one requires explicit new security
work before *any* mutable host-control endpoint ships:

1. **Server Admin gate.** Every endpoint first uses
   `authorize_admin_request` and its existing audit trail. Today, a session
   cookie is `HttpOnly; SameSite=Lax`, with no Origin/`Sec-Fetch-Site`
   enforcement, CSRF token, or step-up/fresh-password concept. The host-control
   mutation design must add all of the following:
   - exact configured-origin validation plus a synchronizer CSRF token for
     browser `POST` requests; this must reject a same-site request from a
     different port as well as a cross-site request;
   - a session-bound, short-lived password-confirmation record, rate-limited
     through the existing password-verification controls;
   - a mutation policy that requires that cookie-backed, recently confirmed
     session and rejects a bare `admin_token` header for `prepare` and
     `confirm`. A token may still authorize read-only capability discovery;
   - typed target confirmation and one-time challenge consumption.

   Setup mode must never expose host actions. These are additive extensions to
   the current admin session model, not properties already supplied by it.
2. **Local-agent gate.** The agent authenticates the calling *local process*,
   not an HTTP bearer token. It accepts only the installed Server Node service
   identity through OS peer credentials and rejects all other local clients.

Host control is off by default and must be explicitly enabled by a local
installer/configuration policy. Remote browser use should require a separate
explicit opt-in in addition to the normal Server Admin exposure policy. The
initial default should be: no agent installed or no actions enabled means no
host mutation is possible through BerryKeep.

The agent records the logical BerryKeep admin/session correlation ID supplied
by the SDK for auditability, but it must not treat that value as an OS
credential. The privileged authorization decision remains the combination of
authenticated IPC peer plus the installation policy.

### Cluster and data-safety preflight

Before it creates a confirmation challenge, the Server Node reports:

- current node identity, platform, and target service identity;
- in-flight write, repair, import, or control-plane operations;
- cluster reachability, replica health, and whether this node is the only
  currently reachable control-plane endpoint;
- whether a graceful node drain is available.

The first implementation should block only hard safety conflicts, such as an
already accepted host action or a state migration that cannot be interrupted.
It should otherwise show clear risk warnings and require an explicit
acknowledgement. Do not make a broken node impossible to restart merely
because its health report is degraded.

A later controlled-maintenance mode can mark the node draining, wait for a
safe replica/control-plane condition, and then send the same host-control
request. It should remain a distinct operator choice, not hidden behavior of
the basic restart button.

### Disconnect and recovery behaviour

The API must flush `202 Accepted` before the agent's bounded delay expires.
The browser then switches to a reconnect screen that polls the node health
endpoint with exponential backoff. It must not show a false terminal success
when the connection closes.

After a service restart or host boot, the Server Node reads the agent journal
and exposes the last action outcome and correlation ID. The UI can then show
"node returned after requested restart" or a diagnostic state rather than
guessing from the original request.

## Platform implementation plan

### Ubuntu (systemd)

Package a root-owned `berrykeep-host-control` system service and a Unix-domain
socket, for example `/run/berrykeep/host-control.sock`.

- A systemd socket unit owns the socket with a restrictive mode and the
  `berrykeep-server-node` identity as its only permitted peer.
- The agent checks `SO_PEERCRED`; filesystem permissions alone are not the
  authorization boundary.
- It uses the system bus with fixed D-Bus calls:
  `org.freedesktop.systemd1.Manager.RestartUnit` for exactly
  `berrykeep-server-node.service`, and
  `org.freedesktop.login1.Manager.Reboot` for a host reboot.
- It does not invoke an arbitrary `systemctl` command or accept a unit name
  from the SDK.
- The package hardens the agent with no network listener, a narrow writable
  state directory, private temporary files, restrictive address families, and
  a systemd sandbox reviewed against the exact D-Bus implementation.
- The agent writes an audit event to journald and its persistent action journal
  before initiating the action.

The current service already receives `SIGTERM` through systemd and the SDK has
a graceful listener shutdown path. Validate the configured timeout against
the node's maximum shutdown work before enabling the UI action.

### macOS (launchd)

Extend the signed server-node package with a root-owned,
installer-provisioned `LaunchDaemon`, for example
`io.berrykeep.host-control`.

- Keep `io.berrykeep.server-node` running as `_berrykeep`; do not change it to
  root.
- Use a root-owned Unix-domain socket such as
  `/var/run/berrykeep/host-control.sock` and validate the connecting UID with
  `getpeereid`/equivalent credentials against `_berrykeep`.
- Restart only the fixed launchd job
  `system/io.berrykeep.server-node` through a fixed native implementation or a
  fixed-argument `launchctl kickstart -k` invocation. Never pass through
  operator text.
- Reboot only through the root helper's fixed coordinated reboot primitive,
  after the accepted-response delay.
- Code-sign and notarize the new binary and package it with root-owned,
  non-writable executable and plist paths. Verify the helper's identity during
  installation and upgrade.

The current macOS deployment is already a system `LaunchDaemon`, so this
approach fits its headless, no-logged-in-user lifecycle. It must not use the
deprecated `AuthorizationExecuteWithPrivileges` path or make the web UI a
privileged GUI application. For the current standalone package, use the
installer-provisioned, root-owned launchd path; if a future app-bundle
installation targets macOS 13 or later, use `SMAppService` for the helper
registration. Treat `SMJobBless` only as a legacy-compatibility option, not
as the preferred new implementation.

### Windows (Service Control Manager)

Extend the MSI with a separate `BerryKeepHostControl` Windows service.

- Run the broker as `LocalSystem` only because it needs controlled host
  privileges; keep `BerryKeepServerNode` as `LocalService`.
- Give both services service SIDs. Expose a named pipe such as
  `\\.\pipe\BerryKeepHostControl` whose ACL permits only the
  `NT SERVICE\BerryKeepServerNode` SID. Also impersonate/check the pipe
  client token before accepting a request.
- Restart only the fixed `BerryKeepServerNode` service through the Service
  Control Manager APIs (`ControlService`, `QueryServiceStatusEx`, and
  `StartServiceW`). Do not call `Restart-Service` or `sc.exe` through a
  shell.
- Reconcile SCM failure actions instead of assuming the broker exclusively
  owns the start. The current MSI configures automatic restart after a failed
  service exit. After a stop request, the broker must observe
  `STOP_PENDING`/ `STOPPED`, read the configured recovery delay, and watch
  through that recovery window. If SCM has returned the service to
  `RUNNING`, record automatic recovery as the successful restart; only call
  `StartServiceW` when it remains stopped. Treat
  `ERROR_SERVICE_ALREADY_RUNNING` as a reconciliation signal, then make the
  final result from `QueryServiceStatusEx`.
- Do not temporarily rewrite the service's failure-action configuration to
  make this work. That would require broader service-configuration authority
  and create another race; bounded observation and final-state reconciliation
  are the safer protocol.
- Request a reboot with the native shutdown API after enabling the required
  shutdown privilege, a non-zero grace period, an explicit planned-maintenance
  reason, and no forced closing of user applications by default.
- Persist the journal under the broker's protected ProgramData directory and
  write a corresponding Windows Event Log event.

The existing Server Node already accepts `SERVICE_CONTROL_STOP` and forwards
it to `run_from_env_with_shutdown`, so a controlled SCM restart can use the
node's graceful shutdown path. That shutdown can still return an error, which
is why the broker must reconcile the MSI's recovery policy before journaling a
result. A Windows reboot request is likewise asynchronous: operating-system
acceptance is not proof that the machine has rebooted.

### Android

Treat Android as two capability tiers.

#### Tier 1: restart the BerryKeep node runtime

This capability is available to the existing Android Server Node app without
device-owner privileges.

- Add an application-internal `HostLifecycleController` owned by
  `ServerNodeForegroundService`.
- Give `EmbeddedManagedServerNodeConfig` a caller-supplied shutdown signal (or
  equivalent managed run handle), so the Android bridge can ask the SDK to
  drain listeners and persist state gracefully. Replace the current task abort
  in `apps/android-server-node-app` with this path, retaining a documented
  last-resort timeout only if graceful shutdown hangs.
- Let the SDK's Android host-control provider enqueue an internal, explicit,
  non-exported service command *after* the HTTP response has been sent. The
  foreground service stops the managed runtime, waits for it to exit, and
  starts it again with the same data directory and bind address.
- Keep the action journal in the app's no-backup node state and expose its
  result after the embedded UI reconnects.

Label this action **Restart BerryKeep node**, not **Restart Android service**:
the Android foreground service remains the lifecycle owner while only the
embedded Rust node runtime is restarted.

#### Tier 2: reboot the Android device

Ordinary Android applications cannot safely or legitimately reboot a user's
device. Advertise `reboot_host` only if all of the following are true:

- the app is provisioned as the device's **Device Owner** during managed-device
  enrollment;
- a `DeviceAdminReceiver` and the required device-policy configuration are
  installed;
- `DevicePolicyManager.isDeviceOwnerApp()` confirms the state at runtime;
- the app can request a reboot through `DevicePolicyManager.reboot()`.

For that managed-appliance tier, add boot handling that restores the foreground
service after `BOOT_COMPLETED`, subject to the Android version's foreground
service rules. A device reboot may be rejected while a phone call is active;
surface that returned condition instead of retrying blindly.

For normal consumer devices, rooted devices, ADB, and shell workarounds, the
capability remains unavailable. Do not add a root-shell fallback: it is neither
portable nor supportable.

## Server Admin UX

Create a shared `HostControlsCard` and mount it where it is useful:

- in the future/current swap or replacement flow;
- next to configuration changes that already report `restart required`;
- optionally on the Dependencies page as the authoritative capability status.

The card should:

1. show host identity, OS, service identity, and available actions;
2. show unavailable reasons rather than hiding unsupported actions;
3. show the cluster/preflight warning before a destructive action;
4. require a fresh admin password confirmation and typed target confirmation;
5. display a short countdown and an explicit "connection will be lost" notice;
6. turn into a reconnect/status view once the request is accepted;
7. show the persisted last-action record after reconnection.

Do not put an always-visible one-click reboot icon in the global navigation.
The reboot control belongs behind the same deliberately worded confirmation
flow as an irreversible maintenance action.

## Delivery sequence

Follow PACE with one independently shippable slice at a time.

1. **Security and product decision**
   - Threat-model a remote Server Admin compromise, local untrusted users, and
     a compromised node process.
   - Decide the default enablement, remote-browser policy, confirmation factor,
     local action-journal retention, cluster hard-block conditions, and the
     Android Device Owner support boundary.
   - Update the Cockpit strategy only after this decision is accepted.

2. **Shared protocol, authorization extension, and read-only UI**
   - Add typed protocol models, a fake provider, SDK capability endpoint,
     generated web API types, and `HostControlsCard` read-only states.
   - Extend `authorize_admin_request` and the existing admin audit flow for
     host-control actions; add exact-Origin validation, synchronizer CSRF
     tokens, a session-bound fresh-password confirmation, and the one-time
     challenge store. Explicitly reject header-token-only mutation requests.
   - Test those security primitives before adding any mutable route. Existing
     `SameSite=Lax` cookies and the generic `approve` flag are not enough
     for this operation.
   - Add no privileged agent and no mutation in this slice.
   - Establish contract tests and visual states for absent, disabled, and
     supported providers.

3. **Ubuntu service restart**
   - Package the socket-activated agent, peer-credential validation, fixed
     systemd restart request, journal, audit records, and `202` reconnect flow.
   - Gate it behind explicit local enablement.
   - Test an actual service restart in an Ubuntu integration environment.

4. **Windows service restart**
   - Add the broker service, service-SID/named-pipe ACL, SCM implementation,
     MSI upgrade handling, and Windows Event Log records.
   - Verify graceful stop/start using the existing SCM signal handler and
     reconcile the existing SCM automatic-recovery delay before assigning a
     terminal broker outcome.

5. **macOS service restart**
   - Add the signed launchd helper, Unix peer authentication, package upgrade
     handling, and launchd restart implementation.
   - Verify on both Apple Silicon and Intel package paths.

6. **Android node-runtime restart**
   - Implement the graceful embedded-run handle, foreground-service lifecycle
     controller, internal bridge, and instrumentation coverage.

7. **Host reboot opt-in**
   - Add the reboot action separately for Ubuntu, Windows, and macOS after
     service restart is proven.
   - Add Android Device Owner support as a separately provisioned appliance
     feature, not as a prerequisite for the ordinary Android app.

8. **Cluster-aware maintenance and documentation**
   - Add optional drain orchestration only after the basic action path is
     robust.
   - Replace stale Cockpit-only wording in Server Admin, package manuals, and
     the existing strategy document while retaining Cockpit as an alternative.

## Test and acceptance plan

Every slice needs narrow tests before broader platform validation.

### Common tests

- protocol serialization/versioning and rejection of every unknown action;
- idempotency, expired/consumed confirmation challenges, and journal recovery;
- HTTP authentication, exact-Origin and CSRF protection (including a
  same-site/different-port request), fresh-password requirement, setup-mode
  denial, bare-token mutation denial, and remote-policy denial;
- reuse of `authorize_admin_request` and the existing admin audit trail for
  both prepare and confirm, linked to the agent journal by correlation ID;
- UI states for unavailable capability, preflight warning, accepted action,
  reconnecting, and recovered result;
- assertions that request payloads cannot represent an arbitrary command or
  service name.

### Privilege-boundary tests

- an unauthorized Unix-socket peer, macOS peer UID, or Windows named-pipe SID
  is rejected;
- an agent with no explicit enablement reports capability unavailable;
- the node service account cannot directly reboot the host or manage arbitrary
  services;
- no agent opens a TCP listener;
- audit records include action, target, timestamp, correlation ID, result, and
  platform error code without storing an admin password or session secret.

### Platform tests

- **Ubuntu:** package installation, systemd socket credentials, a real
  `berrykeep-server-node.service` restart, journal persistence, and reboot
  request behavior in a disposable VM.
- **macOS:** signed package structure, launchd registration, `_berrykeep` peer
  rejection/acceptance, `kickstart` restart, and both architecture packages.
- **Windows:** MSI install/upgrade, named-pipe ACL/SID tests, SCM stop/start,
  Event Log record, recovery-policy reconciliation after an intentional stop
  that exits with an error, and reboot API error mapping in a native Windows
  runner.
- **Android:** instrumentation test for runtime restart and state retention;
  managed-device/device-owner reboot testing on a dedicated test fleet because
  a normal emulator/app installation is not sufficient evidence.

Success means that each supported platform can restart only its own BerryKeep
node from an explicitly confirmed admin UI action, that a reboot appears only
where the platform and local policy allow it, and that neither action adds a
network-reachable privileged endpoint.

## Rollout and rollback

Ship the host agent and UI disabled by default. Enable restart-node first;
enable host reboot only in a later, separate release/setting. Existing installs
without the agent remain fully supported and simply report that host control is
unavailable.

If a platform agent is removed, disabled, or fails health checks, the Server
Node continues operating normally and the UI reverts to an unavailable state.
Cockpit and the operating system's normal administration tools remain valid
fallbacks throughout rollout.

## External platform references

- systemd's manager D-Bus API and logind power-management API:
  <https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.systemd1.html>
  and <https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html>
- Apple Service Management and `launchd` daemon guidance:
  <https://developer.apple.com/documentation/servicemanagement/smappservice>
  and <https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html>.
  `SMJobBless` is documented at
  <https://developer.apple.com/documentation/servicemanagement/smjobbless%28_%3A_%3A_%3A_%3A%29>
  only for legacy compatibility; it is deprecated in favor of `SMAppService`.
- Windows Service Control Manager access rights and controlled shutdown API:
  <https://learn.microsoft.com/en-us/windows/win32/services/service-security-and-access-rights>
  and <https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-initiatesystemshutdownexw>
- Android Device Owner reboot restrictions:
  <https://developer.android.com/work/dpc/device-management.html>
