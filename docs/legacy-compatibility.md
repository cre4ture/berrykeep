# Legacy compatibility

This is the only documentation page that names **IronMesh**. BerryKeep is the
canonical name for every new command, package, setting, protocol value, source
symbol, asset, and document. The persistent identifiers below are retained only
where changing them would strand a supported deployment or immutable platform
identity.

## Client cutover

Desktop and Android clients have no automatic migration. Before installing
BerryKeep, uninstall the IronMesh client package or application, remove its
GNOME Shell extension, and install BerryKeep as a fresh client. On Android,
remove the old app's Documents Provider before reinstalling and grant the new
provider access again. Reauthenticate and recreate every local service, sync
root, and desktop integration; BerryKeep does not migrate or restore former
client configuration, credentials, sync-profile state, or local services.
Defensive cleanup still suppresses former internal artifacts so they cannot be
uploaded if a directory is reused accidentally. Clear the former browser site
data before using the BerryKeep web client.

This deliberate client boundary keeps the server-node compatibility contracts
below small and independently testable. It does not apply to active server-node
or rendezvous deployments.

## Active compatibility contracts

| Former contract | Canonical BerryKeep alternative | Compatibility handling |
| --- | --- | --- |
| `ironmesh-*` Debian packages, commands, systemd units, users, configuration, and state paths | `berrykeep-*` packages, commands, units, users, configuration, and state paths | Transitional packages retain the former package and service contracts while installing and preferring the canonical binaries and paths. |
| `/apt/ironmesh` repository and signing-key paths | `/apt/berrykeep` repository and signing-key paths | The former repository remains a signed mirror for existing APT source entries. |
| `IRONMESH_*` environment variables | `BERRYKEEP_*` environment variables | The shared compatibility reader gives the canonical setting precedence and falls back to the former prefix only when it is absent. |
| Former GitHub Actions signing-secret and repository-variable names | BerryKeep-named workflow environment variables | Repository settings are not renamed by a source change. Release workflows bind the former setting names to canonical environment variables so signing keeps working until settings can be migrated separately. |
| `urn:ironmesh:*` certificate identity SANs | `urn:berrykeep:*` certificate identity SANs | New certificates carry canonical and former URI SANs during the compatibility window; node and rendezvous authentication parse both forms so either rollout direction can renew in place. |
| `x-ironmesh-*` signed-request, admin-token, ingestion-token, and web-service-proxy headers, plus the v1 request-signing context | `x-berrykeep-*` headers and the BerryKeep v1 request-signing context | New requests, proxy responses, and client parsing use canonical headers and signatures. Upgraded servers accept former request headers and either signing context verifies during the compatibility window. |
| `.ironmesh*` internal sync artifacts, former hidden Windows adapter files, and `user.ironmesh.*` Linux FUSE xattrs | BerryKeep-named internal artifacts, files, and xattrs | Rust and Android SAF scanning suppress former artifacts so they cannot become sync traffic. The Windows adapter also excludes former hidden files; Linux FUSE accepts former xattrs only to avoid exposing stale metadata. Former conflict copies remain readable solely to resolve an in-flight conflict. None of these paths restore client configuration or state. |
| The v1 `ironmesh` content-fingerprint domain | Canonical content-fingerprint APIs | The opaque storage-key domain remains unchanged to retain media-analysis and thumbnail caches across an upgrade; it is never surfaced as a product-facing identifier. |
| `IronmeshIosBytes` and `IronmeshInfo` Rust symbols | `BerryKeepIosBytes` and `BerryKeepInfo` | Deprecated Rust aliases keep source consumers building while directing new code to the canonical names. |
| `ironmesh-*.exe` Windows execution aliases and the `UlrichHornung.IronMesh` MSIX identity | `berrykeep-*.exe` execution aliases and BerryKeep display identity | The packaged application retains its previous identity and aliases so Windows upgrades and invocations continue to work; all visible names and primary aliases are BerryKeep. |
| `io.ironmesh.android` and `io.ironmesh.servernode.android` Android application IDs | `io.berrykeep.android` and `io.berrykeep.servernode.android` source namespaces and BerryKeep app display identities | The former Android client package identity remains only while its distribution channel is migrated. It is not a promise to retain client private data, Keystore entries, or provider grants; users follow the fresh-install procedure above. |
| The former Android Documents Provider root ID and exported column namespace | BerryKeep Documents Provider root ID and column namespace | The provider exposes only canonical contracts. Reinstalling the client and granting the provider again replaces the former root; no alias is supplied. |
| `ironmesh.device-identity.aes-gcm.v1`, legacy Android preference files, and share-capability store | BerryKeep-named Keystore alias, preference files, and share-capability store | Android client state is not an external migration contract. The manual fresh-install procedure removes prior client private storage before BerryKeep initializes canonical state. |
| The Android server-node data directory | BerryKeep-named server-node source namespaces and display identity | The retained Android server-node application identity keeps supported node state available across the package rename. |
| `ironmesh-client-gallery-cache` IndexedDB data and `ironmesh.*` browser gallery/theme preferences | BerryKeep-named browser cache and preferences | Gallery cache data is derived and browser display preferences are non-critical, so the web client initializes canonical storage rather than retaining a browser-storage migration. Users can clear the former origin storage through their browser's site-data controls, and reapply display preferences in the BerryKeep UI. |
| `.ironmesh-client-identity.json`, `.ironmesh-connection.json`, and `.ironmesh-remote-snapshot.json` | BerryKeep-named internal Windows files in the canonical local-app-data state root | The Windows adapter writes only canonical files and keeps former internal filenames excluded from sync traffic; client state is recreated through the fresh-install procedure. |
| `dev.ironmesh.apple.*` bundle IDs, app group, Keychain access group, and File Provider domain | BerryKeep target names, visible app name, source modules, and configuration keys | Apple uses these identifiers as the signed update, shared-container, Keychain, and File Provider identities. Released BerryKeep app targets retain them so existing installations update without losing protected or shared data. |
| Former Apple connection-state, Keychain-service, preferences, share-capability, and sync-profile storage | BerryKeep-named state, Keychain service, preferences, share-capability, and profile storage | Readable state is moved to canonical storage. Existing File Provider domains keep their former identifiers because their materialized and queued files cannot be renamed in place. |

## Rules for new work

- Do not add the former name outside an implementation of one of the contracts above.
- Add the canonical BerryKeep contract first, then add a narrowly scoped compatibility reader, alias, or migration only when an existing deployment needs it.
- Canonical input takes precedence whenever both forms are supplied.
- Remove an entry and its implementation together once the compatibility window closes.
