//! Coarse file-type classification by extension, for color-coding the treemap.
//!
//! Deliberately a *small* set of broad categories — the point is "at a glance,
//! what kind of stuff is eating this space" (video vs images vs archives), not a
//! MIME database. OS-agnostic and unit-tested (D6). Colors live in the app (a UI
//! concern); this module only decides the category.

/// Broad content categories used for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCategory {
    Video,
    Image,
    Audio,
    Archive,
    Document,
    Code,
    System,
    Other,
}

impl FileCategory {
    /// Number of categories.
    pub const COUNT: usize = Self::ALL.len();

    /// All categories, in legend order.
    pub const ALL: [FileCategory; 8] = [
        FileCategory::Video,
        FileCategory::Image,
        FileCategory::Audio,
        FileCategory::Archive,
        FileCategory::Document,
        FileCategory::Code,
        FileCategory::System,
        FileCategory::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileCategory::Video => "Video",
            FileCategory::Image => "Image",
            FileCategory::Audio => "Audio",
            FileCategory::Archive => "Archive",
            FileCategory::Document => "Document",
            FileCategory::Code => "Code",
            FileCategory::System => "System",
            FileCategory::Other => "Other",
        }
    }
}

/// Classify a file by its name's extension. Names with no extension (or a
/// leading-dot name like `.gitignore`) are [`FileCategory::Other`].
pub fn categorize(name: &str) -> FileCategory {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    let Some(ext) = ext else {
        return FileCategory::Other;
    };

    use FileCategory::*;
    match ext.as_str() {
        // Video
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg"
        | "m2ts" | "ts" | "vob" | "3gp" | "ogv" | "rm" | "rmvb" | "divx" | "mts" => Video,
        // Image
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "heic" | "heif"
        | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" | "svg" | "ico" | "jfif" | "avif" => Image,
        // Audio
        "mp3" | "flac" | "wav" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "alac" | "aiff"
        | "aif" | "ape" | "mid" => Audio,
        // Archive (incl. comic archives — cbz/cbr — and disc images)
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cbz" | "cbr"
        | "iso" | "cab" | "lz" | "lzma" => Archive,
        // Document / ebook / office
        "pdf" | "epub" | "mobi" | "azw" | "azw3" | "djvu" | "doc" | "docx" | "txt" | "md"
        | "rtf" | "odt" | "xls" | "xlsx" | "csv" | "ppt" | "pptx" => Document,
        // Code / config / markup.
        // NOTE: `.ts` is treated as Video (MPEG transport stream), not
        // TypeScript — the media-collection use case wins the tie.
        "rs" | "py" | "js" | "jsx" | "tsx" | "c" | "h" | "cpp" | "hpp" | "cc" | "go"
        | "rb" | "java" | "kt" | "swift" | "php" | "sh" | "bash" | "ps1" | "json" | "xml"
        | "yaml" | "yml" | "toml" | "html" | "css" | "scss" | "sql" | "lua" => Code,
        // System / binary (mostly what fills a Windows system drive).
        "exe" | "dll" | "sys" | "msi" | "so" | "dylib" | "bin" | "lib" | "obj" | "o" | "a"
        | "pdb" | "ocx" | "drv" | "dmp" | "pak" | "dat" | "cache" | "winmd" => System,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::FileCategory::*;
    use super::*;

    #[test]
    fn classifies_common_types() {
        assert_eq!(categorize("movie.MKV"), Video); // case-insensitive
        assert_eq!(categorize("photo.jpg"), Image);
        assert_eq!(categorize("song.flac"), Audio);
        assert_eq!(categorize("Chapter 1.cbz"), Archive); // manga
        assert_eq!(categorize("book.epub"), Document);
        assert_eq!(categorize("main.rs"), Code);
    }

    #[test]
    fn no_extension_is_other() {
        assert_eq!(categorize("README"), Other);
        assert_eq!(categorize(".gitignore"), Other);
        assert_eq!(categorize("weird.qwerty"), Other);
    }
}
