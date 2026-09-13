# Debian package-name transition

Release artifacts and primary Debian packages use the `berrykeep` name. The
following packages replace their previous names:

| New primary package | Transitional package |
| --- | --- |
| `berrykeep-client` | `berrykeep-client` |
| `berrykeep-server-node` | `berrykeep-server-node` |
| `berrykeep-server-node-map-tools` | `berrykeep-server-node-map-tools` |
| `berrykeep-rendezvous-service` | `berrykeep-rendezvous-service` |

For an existing installation, run the usual `apt update` and `apt upgrade`.
The transitional package pulls in a compatible `berrykeep-*` replacement, and
the legacy service, configuration path, and executable command continue to work.
Its dependency uses the upstream version rather than an architecture-specific
Debian revision, so independently rebuilt suite packages remain upgradeable.
The replacement packages also provide `berrykeep-*` executable symlinks during
the transition window.

New installations should install a `berrykeep-*` package and configure its
`/etc/berrykeep/*.env` file before enabling the matching `berrykeep-*`
systemd unit. Do not remove a transitional package from an upgraded host until
its service has been intentionally migrated to the new configuration and state
paths. Starting a new `berrykeep-server-node.service` or
`berrykeep-rendezvous-service.service` stops its matching legacy service, so
the two identities cannot run simultaneously during that migration.

The client package installs its executable payload under
`/usr/lib/berrykeep-client`; the public `berrykeep` and legacy `berrykeep`
commands remain available in `/usr/bin` during the transition.
`/usr/lib/berrykeep-client` remains a compatibility directory containing aliases
to the new payload and extension assets, so an already-running legacy config
app can complete its package-upgrade handoff.
