// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use i18n_embed::{
    DefaultLocalizer, LanguageLoader, Localizer,
    fluent::{FluentLanguageLoader, fluent_language_loader},
};
use rust_embed::RustEmbed;
use std::sync::LazyLock;

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

pub static LANGUAGE_LOADER: LazyLock<FluentLanguageLoader> = LazyLock::new(|| {
    let loader: FluentLanguageLoader = fluent_language_loader!();

    loader
        .load_fallback_language(&Localizations)
        .expect("Error while loading fallback language");

    loader
});

#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::localize::LANGUAGE_LOADER, $message_id)
    }};

    ($message_id:literal, $($args:expr),*) => {{
        i18n_embed_fl::fl!($crate::localize::LANGUAGE_LOADER, $message_id, $($args), *)
    }};
}

// Get the `Localizer` to be used for localizing this library.
pub fn localizer() -> Box<dyn Localizer> {
    Box::from(DefaultLocalizer::new(&*LANGUAGE_LOADER, &Localizations))
}

pub fn localize() {
    let localizer = localizer();
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    if let Err(error) = localizer.select(&requested_languages) {
        eprintln!("Error while loading language for Window Layout {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_debug_binary_embeds_fallback_translations() {
        // The installer relocates the workspace after compiling. An owned Cow
        // here means rust-embed read the source filesystem at runtime instead.
        let resource = Localizations::get("en/cosmic_applet_tiling.ftl")
            .expect("English fallback must be available in an installed binary");
        assert!(matches!(resource.data, std::borrow::Cow::Borrowed(_)));
        let text = std::str::from_utf8(&resource.data).unwrap();
        for key in ["floating =", "tiling =", "scrolling ="] {
            assert!(
                text.lines().any(|line| line.starts_with(key)),
                "missing {key}"
            );
        }
    }
}
