use anyhow::{Context, Result, anyhow, bail};
use jni::{
    JNIEnv,
    objects::{GlobalRef, JObject},
};
use std::sync::{Mutex, OnceLock};

fn application_context_state() -> &'static OnceLock<GlobalRef> {
    static STATE: OnceLock<GlobalRef> = OnceLock::new();
    &STATE
}

fn initialization_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

/// Publishes a process-lifetime Android application context to Iroh's DNS resolver.
///
/// Iroh stores the raw JNI pointers in `ndk_context`, so the Java object must be
/// promoted to a global reference and retained for as long as the process can use
/// an Iroh endpoint. The lock makes repeated or concurrent Android component
/// initialization safe even though `ndk_context` itself only accepts one install.
#[allow(unsafe_code)]
pub(crate) fn initialize(env: &mut JNIEnv<'_>, application_context: JObject<'_>) -> Result<()> {
    if application_context.is_null() {
        bail!("Android application context must not be null");
    }

    let _guard = initialization_lock()
        .lock()
        .map_err(|_| anyhow!("Android Iroh context initialization lock poisoned"))?;
    if application_context_state().get().is_some() {
        return Ok(());
    }

    let java_vm = env
        .get_java_vm()
        .context("failed to capture Java VM for Iroh DNS")?;
    let application_context = env
        .new_global_ref(application_context)
        .context("failed to retain Android application context for Iroh DNS")?;

    // SAFETY: `JavaVM` belongs to this process and remains valid for its lifetime.
    // The `GlobalRef` is stored in a process-lifetime `OnceLock` immediately after
    // installation and the initialization lock prevents a second install.
    unsafe {
        transport_sdk::install_android_jni_context(
            java_vm.get_java_vm_pointer().cast(),
            application_context.as_obj().as_raw().cast(),
        );
    }

    application_context_state()
        .set(application_context)
        .map_err(|_| anyhow!("Android Iroh context was initialized concurrently"))?;
    Ok(())
}
