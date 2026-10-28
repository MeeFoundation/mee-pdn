//! The one call an Android host makes beside the exported surface,
//! `uniffi.pdn_mobile.AndroidContext.install(context)` by JNI, before its
//! first bring-up: the runtime's DNS reader asks Android for the active
//! network's resolvers through JNI, and needs the application's context to.
//! The `pdn-mobile/host` spec (the exported surface).

use std::sync::Once;

use jni::{
    errors::ThrowRuntimeExAndDefault,
    objects::{JClass, JObject},
    EnvUnowned,
};

/// `external fun install(context: Any)` on `uniffi.pdn_mobile.AndroidContext`,
/// declared by the application; the first call counts, a later one changes
/// nothing. A host's own call rather than `JNI_OnLoad`: JNA loads the library
/// by `dlopen`, which runs no `JNI_OnLoad`.
#[expect(
    unsafe_code,
    reason = "an exported JNI symbol, and iroh's install takes raw pointers"
)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_uniffi_pdn_1mobile_AndroidContext_install<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    context: JObject<'caller>,
) {
    static INSTALLED: Once = Once::new();
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let vm = env.get_java_vm()?;
            let context = env.new_global_ref(&context)?;
            INSTALLED.call_once(|| {
                // SAFETY: the VM outlives the process's Rust code, and the
                // global reference is leaked by `into_raw`, so both pointers
                // stay valid until the process exits, as iroh requires.
                unsafe {
                    pdn_node::install_android_jni_context(
                        vm.get_raw().cast(),
                        context.into_raw().cast(),
                    );
                }
            });
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}
