// The settings window, with its English and Arabic bundled: ../i18n/{en,ar}/LC_MESSAGES/nowbar-ui.po
// (msgids are the Turkish strings themselves, no context; the Rust side's tables come from the
// same files).
fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("../i18n")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("main.slint", config).expect("ui/main.slint");
}
