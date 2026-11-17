# Client service reachability

The client Web UI's **Connection Paths** page includes **Rendezvous and iroh
relay services**. It checks each configured Rendezvous contact independently
of storage-node connections, including cached cluster-managed contacts and
the original bootstrap recovery contacts. Opening the page runs a check;
checks repeat every 30 seconds while the page is open. **Check services**
runs another check without re-evaluating cluster routes.

For each Rendezvous, the client requests `GET /health` with its configured
trust roots and Rendezvous mTLS identity. A successful HTTP response with
`status: "ok"` is **Reachable**. An HTTP error, malformed health response,
or other reported status is **Unhealthy**. A failed connection or timeout
before receiving an HTTP response is **Unreachable**. The UI includes the
URL, HTTP status, request duration, check timestamp, reported status,
software version when present, and error details.

The client checks each distinct `iroh_relay_public_urls` entry from the
health response using the upstream iroh `GET /ping` probe. Relay requests
use the configured trust roots without forwarding the Rendezvous client
identity. A successful probe is **Reachable**; HTTP failures and connection
failures are reported separately from Rendezvous health. An explicit empty
relay list displays **No iroh relay advertised**. If the health response is
unavailable or lacks the relay-address field, the relay status is **unknown**.
Previously discovered relay addresses are not guessed when discovery fails.

These checks measure HTTP service health from the client runtime. They do
not test storage nodes, allocate relay tickets, establish a relay session,
or test UDP/QAD and NAT traversal. A reachable service can still reject
authentication or forwarding requests later. Each HTTP request, including
its response body, has a three-second timeout; checks use bounded
concurrency and responses are limited to 64 KiB. Redirects are reported as
HTTP failures instead of being followed.

`GET /api/v1/connection-services` (legacy alias:
`GET /api/connection-services`) returns a fresh, uncached snapshot. The
endpoint uses the embedded Web UI's existing session protection. It performs
network I/O outside the runtime lock and does not update route scores,
active endpoints, or historical route diagnostics. Partial endpoint failures
remain in the snapshot; a failure to construct the diagnostic client returns
an API error. The UI keeps previous results with an explicit error message
if a later check fails.
