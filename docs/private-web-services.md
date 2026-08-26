# Private Web Services

BerryKeep clients can open HTTP and HTTPS applications that are reachable only
from a server node's local network. The integrated web proxy uses the existing
authenticated client-to-node transport, including direct QUIC and rendezvous
relay routes. It does not publish the application on a public relay domain and
does not require an inbound connection to the home network.

## Request path

1. An administrator defines a service on the server node that can reach it.
2. The definition fixes the upstream URL, TLS policy, and allowed device IDs.
3. An allowed client lists the opaque service description and asks that exact
   node to open the service ID.
4. The client opens a dedicated multiplexed stream through its selected direct
   or relay route. The server resolves the service ID and connects to the fixed
   upstream. Clients cannot submit a destination host or port.
5. The local client Web UI issues a one-minute, single-use launch link. The
   browser redeems it on a service-specific `*.localhost` origin and
   receives a host-only gateway session cookie.
6. Browser HTTP, streaming uploads/downloads, and WebSocket upgrades are carried
   over authenticated proxy streams. HTTPS is terminated and verified on the
   server node, so the browser sees only its trustworthy loopback origin.

The local listener must remain bound to loopback. Separate service hostnames are
intentional: browser cookies, storage, service workers, and same-origin policy
are isolated from the BerryKeep client UI and from other configured services.
Every alias is a direct child of `.localhost`, so sibling services are distinct
browser sites rather than subdomains of a shared registrable parent. The gateway
also rejects browser requests whose Origin or Fetch Metadata identifies another
site.

## Configure a service

Open **Server Admin → Web Services** on the node with LAN access to the target.
Enter:

- a stable lowercase service ID, such as `home-nas`;
- a display name and optional description;
- the fixed upstream URL, including a non-default port or base path when needed;
- one or more enrolled client device IDs; and
- the upstream HTTPS trust mode.

The service is node-local because LAN reachability and certificate trust are
node-local facts. A client with routes to multiple nodes aggregates the service
list and sends each connection to the node named in its descriptor.

## Existing certificates

New certificates are not required. Choose the narrowest mode that matches the
certificate already used by the application:

- **System trust store** validates a publicly trusted or locally installed CA in
  the normal way, including hostname and expiry checks.
- **Existing CA certificate (PEM)** adds the supplied CA or self-signed trust
  anchor only for this service. Normal hostname and validity checks still run.
- **Exact certificate SHA-256 pin** accepts only the exact leaf certificate DER
  fingerprint configured for this service. This is useful for an existing NAS
  self-signed certificate, including certificates whose name does not match an
  IP-address URL. Replace the pin when that certificate is rotated.

`TLS server name override` can connect to an IP address while sending and, in
the system/CA modes, validating a certificate DNS name. In exact-pin mode it
controls SNI only; server identity remains bound to the pinned certificate.
BerryKeep never exposes a global "accept invalid certificates" option.

Example fingerprint extraction:

```sh
openssl s_client -connect nas.home.arpa:443 -servername nas.home.arpa </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256
```

Verify the fingerprint over a trusted local path before saving it.

## Browser behavior

Use **Client UI → Web services → Open in browser**. The launch token is removed
from the address bar immediately after redemption. The local gateway keeps the
upstream application's cookies on its stable service origin while never
forwarding the BerryKeep gateway cookie upstream. It also rewrites same-origin
redirects, cookie domains/base paths, Origin/Referer, and transport-only security
headers that would incorrectly force local HTTP to HTTPS.

Applications that hard-code unrelated absolute public origins can still leave
the proxy origin; configure the upstream application with root-relative URLs or
its normal canonical base URL where possible. The integrated proxy deliberately
does not rewrite HTML or JavaScript bodies. The upstream must accept HTTP/1.1;
browser-facing HTTP/2 is not required for streaming or WebSocket support. The
gateway keeps a small, short-lived HTTP/1.1 connection pool per node and service
so page assets reuse authenticated proxy and upstream TLS connections.

Nodes that predate this feature are treated as having no configured web
services, so clients and nodes can be upgraded incrementally. Opening a service
still requires the selected client and target node to support the proxy stream.

## Security properties

- Service listings and stream opens require a valid, non-revoked device
  credential.
- Every stream revalidates the signed request and nonce, then applies the
  service's device allowlist.
- Disabled, unknown, and unauthorized services all fail without disclosing the
  configured target.
- The rendezvous service sees only the already protected multiplex transport;
  production relay sessions retain inner mutual TLS.
- Service changes require administrator authentication, are written atomically
  under `state/web_services.json`, and produce administrator audit events.
- Upstream TLS failures fail closed. No plaintext downgrade is attempted.
