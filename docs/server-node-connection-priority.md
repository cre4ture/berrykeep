# Server node connection priority

IronMesh clients can prefer stronger server hardware without turning the preference into a hard routing rule. A node priority is a signed integer from `-20` through `20`:

- `0` is neutral and preserves the previous behavior.
- Positive values make a node more attractive to clients.
- Negative values make a node less attractive while retaining it as a failover route.

Priority is a soft input to route scoring. Each priority step contributes a 25 ms score bias. Measured latency, route failures, circuit breaking, direct-versus-relay selection, and failover continue to apply.

## Server configuration

The node-local admin panel exposes **Hardware → Client connection priority**. The corresponding authenticated API is:

```text
GET /auth/node-connection-priority
PUT /auth/node-connection-priority
Content-Type: application/json

{"priority": 8}
```

When the node was started from a node-enrollment package, the setting is written back to that package. Otherwise it is runtime-only. Environment-based deployments can set `IRONMESH_NODE_CONNECTION_PRIORITY`; the value is validated at startup.

The node advertises the value through its Rendezvous presence under the reserved `client_connection_priority` label. Rendezvous discovery returns it as `node_connection_priority`. Older nodes and Rendezvous services omit the value and are treated as priority `0`.

## Experimental mobile overrides

The iOS and Android settings contain an **Experimental server priorities** section under advanced settings. It lists node IDs observed in the current route snapshot. Enabling a manual override stores the value in the client's connection bootstrap:

```json
{
  "node_priority_overrides": {
    "018f1f74-7b65-7c09-9d13-3a6644d0d999": 7
  }
}
```

A manual value replaces the advertised priority for that node on every direct and relay route. Disabling the override returns the node to its server-advertised value. The overrides are client-local and do not modify server configuration.
