use anyhow::{Context, Result};
use jni::{JNIEnv, objects::JString};

/// Native worker threads have no Java caller to receive a pending exception.
/// Consume it before detaching from the VM, preserving its details as a Rust error.
pub(crate) fn with_java_exception_boundary<T>(
    env: &mut JNIEnv<'_>,
    operation: impl FnOnce(&mut JNIEnv<'_>) -> Result<T>,
) -> Result<T> {
    let result = operation(env);
    let exception = env.exception_occurred()?;
    if exception.is_null() {
        return result;
    }
    env.exception_clear()?;
    let description = (|| -> Result<String> {
        let value = env
            .call_method(&exception, "toString", "()Ljava/lang/String;", &[])?
            .l()?;
        Ok(env.get_string(&JString::from(value))?.into())
    })();
    // Even Throwable.toString can throw. Never let that secondary exception escape.
    env.exception_clear()?;
    let description =
        description.unwrap_or_else(|_| "Java exception (description unavailable)".into());
    match result {
        Ok(_) => Err(anyhow::anyhow!(description)),
        Err(error) => Err(error).context(description),
    }
}
