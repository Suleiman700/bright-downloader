//! File-type categories, used for the sidebar filter and per-category folders.

pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("Compressed", &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "cab", "arj", "lzh"]),
    ("Documents", &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "odt", "ods", "odp", "epub", "csv", "md"]),
    ("Music", &["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "aif", "aiff", "mid"]),
    ("Programs", &["exe", "msi", "msix", "appx", "apk", "dmg", "pkg", "deb", "rpm", "appimage", "bat", "jar"]),
    ("Video", &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "3gp", "ts", "m2ts"]),
];

pub const GENERAL: &str = "General";

pub fn category_for(file_name: &str) -> &'static str {
    let Some((_, ext)) = file_name.rsplit_once('.') else { return GENERAL };
    let ext = ext.to_ascii_lowercase();
    CATEGORIES
        .iter()
        .find(|(_, exts)| exts.contains(&ext.as_str()))
        .map_or(GENERAL, |(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_extensions() {
        assert_eq!(category_for("setup.EXE"), "Programs");
        assert_eq!(category_for("movie.final.mkv"), "Video");
        assert_eq!(category_for("archive.tar.gz"), "Compressed");
        assert_eq!(category_for("README"), GENERAL);
        assert_eq!(category_for("weird.xyz"), GENERAL);
    }
}
