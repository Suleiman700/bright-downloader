//! The few texts the app shows itself (notifications, tray menu, quit
//! prompt); the window translates its own texts.

/// Supported UI languages besides English.
pub const LANGUAGES: &[&str] = &["en", "ar", "fr", "es"];

/// "auto" → the system language if supported, else English.
pub fn resolve(setting: &str) -> &'static str {
    let wanted = if setting == "auto" || setting.is_empty() { system_language() } else { setting.to_ascii_lowercase() };
    let base = wanted.split(['-', '_']).next().unwrap_or("en");
    LANGUAGES.iter().copied().find(|l| *l == base).unwrap_or("en")
}

#[cfg(windows)]
fn system_language() -> String {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    let mut buf = [0u16; 85];
    // SAFETY: the buffer is LOCALE_NAME_MAX_LENGTH long.
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if n > 1 {
        String::from_utf16_lossy(&buf[..n as usize - 1])
    } else {
        "en".into()
    }
}

#[cfg(not(windows))]
fn system_language() -> String {
    std::env::var("LANG").unwrap_or_else(|_| "en".into())
}

/// `key` in `lang`; `{0}` in the result is replaced by `arg`.
pub fn t(lang: &str, key: &str, arg: &str) -> String {
    let text = match (lang, key) {
        ("ar", "complete") => "اكتمل التنزيل",
        ("fr", "complete") => "Téléchargement terminé",
        ("es", "complete") => "Descarga completada",
        (_, "complete") => "Download complete",
        ("ar", "failed") => "فشل التنزيل",
        ("fr", "failed") => "Échec du téléchargement",
        ("es", "failed") => "La descarga falló",
        (_, "failed") => "Download failed",
        ("ar", "copied") => "تم نسخ رابط",
        ("fr", "copied") => "Lien copié",
        ("es", "copied") => "Enlace copiado",
        (_, "copied") => "Link copied",
        ("ar", "copied_body") => "افتح Bright Downloader من شريط المهام لتنزيل {0}",
        ("fr", "copied_body") => "Ouvrez Bright Downloader depuis la zone de notification pour télécharger {0}",
        ("es", "copied_body") => "Abre Bright Downloader desde la bandeja para descargar {0}",
        (_, "copied_body") => "Open Bright Downloader from the tray to download {0}",
        ("ar", "refreshed") => "تم تحديث عنوان التنزيل",
        ("fr", "refreshed") => "Adresse du téléchargement mise à jour",
        ("es", "refreshed") => "Dirección de descarga actualizada",
        (_, "refreshed") => "Download address updated",
        ("ar", "open") => "فتح Bright Downloader",
        ("fr", "open") => "Ouvrir Bright Downloader",
        ("es", "open") => "Abrir Bright Downloader",
        (_, "open") => "Open Bright Downloader",
        ("ar", "quit") => "خروج",
        ("fr", "quit") => "Quitter",
        ("es", "quit") => "Salir",
        (_, "quit") => "Quit",
        ("ar", "quit_title") => "إنهاء Bright Downloader",
        ("fr", "quit_title") => "Quitter Bright Downloader",
        ("es", "quit_title") => "Salir de Bright Downloader",
        (_, "quit_title") => "Quit Bright Downloader",
        ("ar", "quit_ask") => "هل تريد إنهاء Bright Downloader؟ سيتم إيقاف التنزيلات الجارية مؤقتًا.",
        ("fr", "quit_ask") => "Quitter Bright Downloader ? Les téléchargements en cours seront mis en pause.",
        ("es", "quit_ask") => "¿Salir de Bright Downloader? Las descargas en curso se pausarán.",
        (_, "quit_ask") => "Quit Bright Downloader? Running downloads will be paused.",
        ("ar", "run_failed") => "تعذّر تشغيل البرنامج",
        ("fr", "run_failed") => "Impossible de lancer le programme",
        ("es", "run_failed") => "No se pudo ejecutar el programa",
        (_, "run_failed") => "Could not run the program",
        ("ar", "added") => "تمت الإضافة إلى Bright Downloader",
        ("fr", "added") => "Ajouté à Bright Downloader",
        ("es", "added") => "Añadido a Bright Downloader",
        (_, "added") => "Added to Bright Downloader",
        _ => key,
    };
    text.replace("{0}", arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_english() {
        assert_eq!(resolve("fr"), "fr");
        assert_eq!(resolve("es-MX"), "es");
        assert_eq!(resolve("de"), "en");
        assert_eq!(t("de", "complete", ""), "Download complete");
        assert_eq!(t("ar", "copied_body", "a.zip"), "افتح Bright Downloader من شريط المهام لتنزيل a.zip");
    }
}
