# Legacy compatibility

This is the only documentation page that names **IronMesh**. BerryKeep is the
canonical name for every new command, package, setting, protocol value, source
symbol, asset, and document. The persistent identifiers below are retained only
where changing them would strand an installed client or its protected data.

## Active compatibility contracts

| Former contract | Canonical BerryKeep alternative | Compatibility handling |
| --- | --- | --- |
| `ironmesh-*` Debian packages, commands, systemd units, users, configuration, and state paths | `berrykeep-*` packages, commands, units, users, configuration, and state paths | Transitional packages retain the former package and service contracts while installing and preferring the canonical binaries and paths. |
| `ironmesh-server-node` in the macOS package payload | `berrykeep-server-node` in the BerryKeep macOS package payload | The canonical macOS binary is installed normally and the former executable name is a symlink for existing invocations. |
| `/apt/ironmesh` repository and signing-key paths | `/apt/berrykeep` repository and signing-key paths | The former repository remains a signed mirror for existing APT source entries. |
| `IRONMESH_*` environment variables | `BERRYKEEP_*` environment variables | The shared compatibility reader gives the canonical setting precedence and falls back to the former prefix only when it is absent. |
| `urn:ironmesh:*` certificate identity SANs | `urn:berrykeep:*` certificate identity SANs | New certificates carry canonical SANs; node and rendezvous authentication still parse former SANs so deployed peers can renew in place. |
| `IronMeshClient`, `ManagedIronMeshClient`, `IronmeshIosBytes`, and `IronmeshInfo` Rust symbols | `BerryKeepClient`, `ManagedBerryKeepClient`, `BerryKeepIosBytes`, and `BerryKeepInfo` | Deprecated Rust aliases keep source consumers building while directing new code to the canonical names. |
| `ironmesh-*.exe` Windows execution aliases and the `UlrichHornung.IronMesh` MSIX identity | `berrykeep-*.exe` execution aliases and BerryKeep display identity | The packaged application retains its previous identity and aliases so Windows upgrades and invocations continue to work; all visible names and primary aliases are BerryKeep. |
| `ironmesh-status@ironmesh.io` GNOME extension UUID and installation directory | `berrykeep-status@berrykeep.io` GNOME extension UUID and installation directory | The canonical extension is installed normally; the package also supplies a transformed former UUID only for installed clients. |
| `io.ironmesh.android` and `io.ironmesh.servernode.android` Android application IDs | `io.berrykeep.android` and `io.berrykeep.servernode.android` source namespaces and BerryKeep app display identities | Android treats an application ID as its update and private-data identity. The released BerryKeep apps retain the former IDs so installed clients keep their data, Keystore entries, and document-provider grants. |
| `ironmesh.device-identity.aes-gcm.v1`, legacy Android preference files, share-capability store, and server-node data directory | BerryKeep-named Keystore alias, preference files, share-capability store, and server-node data directory | Android migrates readable state to canonical storage and falls back to the former identifiers only while completing that migration. |
| `dev.ironmesh.apple.*` bundle IDs, app group, Keychain access group, and File Provider domain | BerryKeep target names, visible app name, source modules, and configuration keys | Apple uses these identifiers as the signed update, shared-container, Keychain, and File Provider identities. Released BerryKeep app targets retain them so existing installations update without losing protected or shared data. |
| Former Apple connection-state, Keychain-service, preferences, share-capability, and sync-profile storage | BerryKeep-named state, Keychain service, preferences, share-capability, and profile storage | Readable state is moved to canonical storage. Existing File Provider domains keep their former identifiers because their materialized and queued files cannot be renamed in place. |

## Rules for new work

- Do not add the former name outside an implementation of one of the contracts above.
- Add the canonical BerryKeep contract first, then add a narrowly scoped compatibility reader, alias, or migration only when an existing deployment needs it.
- Canonical input takes precedence whenever both forms are supplied.
- Remove an entry and its implementation together once the compatibility window closes.
