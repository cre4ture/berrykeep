package io.ironmesh.android.data

import com.squareup.moshi.JsonAdapter
import com.squareup.moshi.Moshi
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory

interface DeviceAuthPreferencesStorage {
    fun read(): String?

    fun write(value: String)

    fun clear()
}

class DeviceAuthStatePersistence(
    private val preferences: DeviceAuthPreferencesStorage,
    private val secretStore: DeviceIdentitySecretStore,
    private val codec: DeviceAuthStateCodec = DeviceAuthStateCodec(),
) {
    @Synchronized
    fun load(): DeviceAuthState {
        val decodedState = preferences.read()?.let(codec::decodeWithMigration)
        val persistedState = decodedState?.state ?: DeviceAuthState()
        val protectedSecret = secretStore.load()
        val hasLegacySecret = persistedState.hasSensitiveIdentityMaterial()
        val legacySecret = if (protectedSecret == null && hasLegacySecret) {
            DeviceIdentitySecret.fromState(persistedState)
        } else {
            null
        }
        val effectiveSecret = protectedSecret ?: legacySecret

        if (hasLegacySecret) {
            secretStore.save(requireNotNull(effectiveSecret))
            preferences.write(codec.encode(persistedState.withoutSensitiveIdentityMaterial()))
        }

        if (effectiveSecret == null && persistedState.hasIdentityMetadata()) {
            throw DeviceIdentityRecoveryRequiredException()
        }

        if (!hasLegacySecret && decodedState?.requiresPersistence == true) {
            preferences.write(codec.encode(persistedState.withoutSensitiveIdentityMaterial()))
        }

        val sanitizedState = persistedState.withoutSensitiveIdentityMaterial()
        return effectiveSecret?.applyingTo(sanitizedState) ?: sanitizedState
    }

    @Synchronized
    fun save(state: DeviceAuthState) {
        val secret = DeviceIdentitySecret.fromState(state)
        if (secret == null && state.hasIdentityMetadata()) {
            throw DeviceIdentityStorageException(
                "The device identity is incomplete. Enroll this device again before saving it.",
            )
        }

        val protectedSecret = runCatching { secretStore.load() }.getOrNull()
        if (secret == null) {
            secretStore.clear()
        } else {
            secretStore.save(secret)
        }
        try {
            preferences.write(codec.encode(state.withoutSensitiveIdentityMaterial()))
        } catch (error: Exception) {
            restoreProtectedSecret(protectedSecret, error)
            throw error
        }
    }

    @Synchronized
    fun clear() {
        // Clearing is the recovery path for a corrupt envelope or a lost Keystore key.
        val protectedSecret = runCatching { secretStore.load() }.getOrNull()
        secretStore.clear()
        try {
            preferences.clear()
        } catch (error: Exception) {
            restoreProtectedSecret(protectedSecret, error)
            throw error
        }
    }

    private fun restoreProtectedSecret(
        protectedSecret: DeviceIdentitySecret?,
        originalError: Exception,
    ) {
        try {
            if (protectedSecret == null) {
                secretStore.clear()
            } else {
                secretStore.save(protectedSecret)
            }
        } catch (restoreError: Exception) {
            originalError.addSuppressed(restoreError)
        }
    }
}

class DeviceAuthStateCodec(
    private val adapter: JsonAdapter<DeviceAuthState> = Moshi.Builder()
        .add(KotlinJsonAdapterFactory())
        .build()
        .adapter(DeviceAuthState::class.java),
) {
    private val legacyBootstrapAdapter: JsonAdapter<LegacyConnectionBootstrapState> = Moshi.Builder()
        .add(KotlinJsonAdapterFactory())
        .build()
        .adapter(LegacyConnectionBootstrapState::class.java)

    fun encode(state: DeviceAuthState): String = adapter.toJson(state)

    fun decode(raw: String): DeviceAuthState = decodeWithMigration(raw).state

    internal fun decodeWithMigration(raw: String): DecodedDeviceAuthState =
        try {
            val state = adapter.fromJson(raw)
                ?: throw DeviceIdentityStorageException(
                    "Stored device authentication settings are empty. Clear local enrollment and enroll again.",
                )
            val legacyBootstrap = legacyBootstrapAdapter.fromJson(raw)
                ?.connectionBootstrapJson
                .orEmpty()
                .trim()
            val migratedState = if (
                !state.connectionInput.isConnectionBootstrapJson() &&
                    legacyBootstrap.isConnectionBootstrapJson()
            ) {
                state.copy(connectionInput = legacyBootstrap)
            } else {
                state
            }
            DecodedDeviceAuthState(
                state = migratedState,
                requiresPersistence = migratedState != state,
            )
        } catch (error: DeviceIdentityStorageException) {
            throw error
        } catch (error: Exception) {
            throw DeviceIdentityStorageException(
                "Stored device authentication settings are damaged. Clear local enrollment and enroll again.",
                error,
            )
        }
}

internal data class DecodedDeviceAuthState(
    val state: DeviceAuthState,
    val requiresPersistence: Boolean,
)

private data class LegacyConnectionBootstrapState(
    val connectionBootstrapJson: String? = null,
)

private fun String.isConnectionBootstrapJson(): Boolean = trim().startsWith("{")
