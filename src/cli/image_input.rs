//! Image input handling for the Sned CLI.
//!
//! Ports behavior from `dirac/cli/src/utils/parser.ts`.
//! Loads image files from disk, converts them to base64, and constructs
//! `ImageContentBlock` values for provider requests.

use base64::Engine;
use std::path::Path;

/// Supported image file extensions and their MIME types.
const IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
];

/// Error type for image loading operations.
#[derive(Debug, thiserror::Error)]
pub enum ImageLoadError {
    #[error("File not found: {0}")]
    NotFound(String),
    #[error("Not an image file: {0}")]
    NotImage(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Check if a file path has a supported image extension.
#[must_use] 
pub fn is_image_path(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .any(|(e, _)| e.trim_start_matches('.') == ext)
}

/// Get the MIME type for a given file extension.
fn get_mime_type(file_path: &str) -> &'static str {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .find(|(e, _)| e.trim_start_matches('.') == ext)
        .map_or("image/png", |(_, mime)| *mime)
}

/// Load an image file from disk and convert it to a base64-encoded `ImageContentBlock`.
///
/// Returns `Ok(ImageContentBlock)` on success, or `ImageLoadError` if the file
/// cannot be read or is not a supported image format.
pub fn load_image_to_content_block(
    file_path: &str,
) -> Result<crate::providers::ImageContentBlock, ImageLoadError> {
    let resolved_path = Path::new(file_path);

    if !resolved_path.exists() {
        return Err(ImageLoadError::NotFound(file_path.to_string()));
    }

    if !is_image_path(file_path) {
        return Err(ImageLoadError::NotImage(file_path.to_string()));
    }

    let data = std::fs::read(resolved_path)?;
    let media_type = get_mime_type(file_path).to_string();
    let base64_data = base64::engine::general_purpose::STANDARD.encode(&data);

    Ok(crate::providers::ImageContentBlock {
        source: crate::providers::ImageSource::Base64 {
            media_type,
            data: base64_data,
        },
        shared: crate::providers::SharedContentFields {
            call_id: None,
            signature: None,
        },
    })
}

/// Load multiple image files and return successfully loaded content blocks.
///
/// Files that fail to load are skipped (errors are silently ignored,
/// matching the TypeScript `processImagePaths` behavior).
#[must_use] 
pub fn load_images_to_content_blocks(
    image_paths: &[String],
) -> Vec<crate::providers::ImageContentBlock> {
    image_paths
        .iter()
        .filter_map(|path| match load_image_to_content_block(path) {
            Ok(block) => Some(block),
            Err(e) => {
                tracing::warn!("Failed to load image '{}': {}", path, e);
                None
            }
        })
        .collect()
}

/// Parse a prompt string and extract image file paths.
///
/// Supports `@/path/to/image.png` syntax as well as standalone absolute paths
/// that look like images. Returns the cleaned prompt and extracted paths.
#[must_use] 
pub fn parse_images_from_input(input: &str) -> (String, Vec<String>) {
    let mut image_paths = Vec::new();

    // Match @/path/to/image.ext patterns
    let at_path_regex =
        regex::Regex::new(r"(?:^|\s)@(/[^\s]+\.(?:png|jpg|jpeg|gif|webp))").expect("valid regex");
    for cap in at_path_regex.captures_iter(input) {
        if let Some(m) = cap.get(1) {
            let path = m.as_str().to_string();
            if !image_paths.contains(&path) {
                image_paths.push(path);
            }
        }
    }

    // Match standalone absolute paths that look like images
    let standalone_regex =
        regex::Regex::new(r"(?:^|\s)(/[^\s]+\.(?:png|jpg|jpeg|gif|webp))(?:\s|$)")
            .expect("valid regex");
    for cap in standalone_regex.captures_iter(input) {
        if let Some(m) = cap.get(1) {
            let path = m.as_str().to_string();
            if !image_paths.contains(&path) {
                image_paths.push(path);
            }
        }
    }

    // Remove image references from prompt
    let mut prompt = at_path_regex.replace_all(input, " ").to_string();
    prompt = standalone_regex.replace_all(&prompt, " ").to_string();
    prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");

    (prompt, image_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_is_image_path() {
        assert!(is_image_path("/path/to/image.png"));
        assert!(is_image_path("/path/to/image.jpg"));
        assert!(is_image_path("/path/to/image.jpeg"));
        assert!(is_image_path("/path/to/image.gif"));
        assert!(is_image_path("/path/to/image.webp"));
        assert!(is_image_path("/path/to/image.PNG"));
        assert!(!is_image_path("/path/to/image.txt"));
        assert!(!is_image_path("/path/to/image"));
    }

    #[test]
    fn test_get_mime_type() {
        assert_eq!(get_mime_type("test.png"), "image/png");
        assert_eq!(get_mime_type("test.jpg"), "image/jpeg");
        assert_eq!(get_mime_type("test.jpeg"), "image/jpeg");
        assert_eq!(get_mime_type("test.gif"), "image/gif");
        assert_eq!(get_mime_type("test.webp"), "image/webp");
        assert_eq!(get_mime_type("test.unknown"), "image/png");
    }

    #[test]
    fn test_load_image_not_found() {
        let result = load_image_to_content_block("/nonexistent/path/image.png");
        assert!(matches!(result, Err(ImageLoadError::NotFound(_))));
    }

    #[test]
    fn test_load_image_not_image() {
        let result = load_image_to_content_block("/etc/passwd");
        assert!(matches!(result, Err(ImageLoadError::NotImage(_))));
    }

    #[test]
    fn test_load_image_success() {
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"fake image data").unwrap();
        let _path = temp_file.path().to_str().unwrap();

        // Rename to have .png extension
        let mut temp_file_png = NamedTempFile::new().unwrap();
        temp_file_png.write_all(b"fake image data").unwrap();
        let path_png = temp_file_png.path().with_extension("png");
        std::fs::rename(temp_file_png.path(), &path_png).unwrap();

        let result = load_image_to_content_block(path_png.to_str().unwrap());
        assert!(result.is_ok());

        let block = result.unwrap();
        match block.source {
            crate::providers::ImageSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert!(!data.is_empty());
                // Verify it's valid base64
                let decoded = base64::engine::general_purpose::STANDARD.decode(&data);
                assert!(decoded.is_ok());
                assert_eq!(decoded.unwrap(), b"fake image data");
            }
            _ => panic!("Expected Base64 source"),
        }
    }

    #[test]
    fn test_parse_images_from_input_at_syntax() {
        let input = "analyze this image @/path/to/image.png";
        let (prompt, paths) = parse_images_from_input(input);
        assert_eq!(prompt, "analyze this image");
        assert_eq!(paths, vec!["/path/to/image.png"]);
    }

    #[test]
    fn test_parse_images_from_input_multiple() {
        let input = "compare @/img1.png and @/img2.jpg";
        let (prompt, paths) = parse_images_from_input(input);
        assert_eq!(prompt, "compare and");
        assert_eq!(paths, vec!["/img1.png", "/img2.jpg"]);
    }

    #[test]
    fn test_parse_images_from_input_standalone() {
        let input = "look at /path/to/image.png please";
        let (prompt, paths) = parse_images_from_input(input);
        assert_eq!(prompt, "look at please");
        assert_eq!(paths, vec!["/path/to/image.png"]);
    }

    #[test]
    fn test_parse_images_from_input_no_images() {
        let input = "just some text without images";
        let (prompt, paths) = parse_images_from_input(input);
        assert_eq!(prompt, "just some text without images");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_parse_images_from_input_at_start() {
        let input = "@/start.png is the image";
        let (prompt, paths) = parse_images_from_input(input);
        assert_eq!(prompt, "is the image");
        assert_eq!(paths, vec!["/start.png"]);
    }

    #[test]
    fn test_load_images_to_content_blocks() {
        use tempfile::NamedTempFile;

        let mut file1 = NamedTempFile::new().unwrap();
        file1.write_all(b"image1 data").unwrap();
        let path1 = file1.path().with_extension("png");
        std::fs::rename(file1.path(), &path1).unwrap();

        let mut file2 = NamedTempFile::new().unwrap();
        file2.write_all(b"image2 data").unwrap();
        let path2 = file2.path().with_extension("jpg");
        std::fs::rename(file2.path(), &path2).unwrap();

        let paths = vec![
            path1.to_str().unwrap().to_string(),
            path2.to_str().unwrap().to_string(),
            "/nonexistent.png".to_string(),
        ];

        let blocks = load_images_to_content_blocks(&paths);
        assert_eq!(blocks.len(), 2);

        match &blocks[0].source {
            crate::providers::ImageSource::Base64 { media_type, .. } => {
                assert_eq!(media_type, "image/png");
            }
            _ => panic!("Expected Base64 source"),
        }

        match &blocks[1].source {
            crate::providers::ImageSource::Base64 { media_type, .. } => {
                assert_eq!(media_type, "image/jpeg");
            }
            _ => panic!("Expected Base64 source"),
        }
    }
}
