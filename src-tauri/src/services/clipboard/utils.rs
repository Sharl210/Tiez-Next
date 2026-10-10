use crate::database::save_image_to_file;
use base64::{engine::general_purpose, Engine as _};
use regex::Regex;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use urlencoding::decode;

const RICH_TEXT_PREVIEW_FALLBACK: &str = "[Rich Text Content]";
pub const RICH_IMAGE_FALLBACK_PREFIX: &str = "<!--TIEZ_RICH_IMAGE:";
pub const RICH_IMAGE_FALLBACK_SUFFIX: &str = "-->";
pub const RICH_NAMED_FORMATS_PREFIX: &str = "<!--TIEZ_RICH_FORMATS:";
pub const RICH_NAMED_FORMATS_SUFFIX: &str = "-->";
const REMOTE_IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;
const REMOTE_IMAGE_TIMEOUT_SECS: u64 = 4;

#[derive(Serialize, Deserialize)]
struct StoredNamedClipboardFormat {
    name: String,
    data_base64: String,
}

fn normalize_image_ext(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "bmp" => Some("bmp"),
        _ => None,
    }
}

fn image_ext_from_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

fn image_ext_from_url(url: &str) -> Option<&'static str> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let ext = Path::new(parsed.path())
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    normalize_image_ext(ext)
}

fn image_ext_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    let format = image::guess_format(bytes).ok()?;
    match format {
        image::ImageFormat::Png => Some("png"),
        image::ImageFormat::Jpeg => Some("jpg"),
        image::ImageFormat::Gif => Some("gif"),
        image::ImageFormat::WebP => Some("webp"),
        image::ImageFormat::Bmp => Some("bmp"),
        _ => None,
    }
}

fn image_mime_by_ext(ext: &str) -> &'static str {
    match ext {
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

fn normalize_remote_img_url(src: &str) -> Option<String> {
    let trimmed = src.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_string());
    }
    if trimmed.starts_with("//") {
        return Some(format!("https:{}", trimmed));
    }
    None
}

fn fetch_remote_image(url: &str) -> Option<(Vec<u8>, &'static str)> {
    static REMOTE_IMG_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

    let client = REMOTE_IMG_CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(REMOTE_IMAGE_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    });

    let resp = client.get(url).header("Accept", "image/*").send().ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let content_len = resp.content_length().unwrap_or(0);
    if content_len > REMOTE_IMAGE_MAX_BYTES as u64 {
        return None;
    }

    let mime = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let mut limited = resp.take((REMOTE_IMAGE_MAX_BYTES as u64) + 1);
    let mut bytes = Vec::new();
    if limited.read_to_end(&mut bytes).is_err() {
        return None;
    }
    if bytes.is_empty() || bytes.len() > REMOTE_IMAGE_MAX_BYTES {
        return None;
    }

    let ext = image_ext_from_mime(&mime)
        .or_else(|| image_ext_from_url(url))
        .or_else(|| image_ext_from_bytes(&bytes))?;

    Some((bytes, ext))
}

fn normalize_html_image_src_candidate(src: &str) -> Option<String> {
    let trimmed = src.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = if trimmed.starts_with("data:") {
        trimmed.to_string()
    } else {
        let first_candidate = trimmed
            .split(',')
            .next()
            .unwrap_or(trimmed)
            .split_whitespace()
            .next()
            .unwrap_or(trimmed)
            .trim();
        first_candidate.replace("&amp;", "&")
    };

    if normalized.is_empty()
        || normalized.starts_with("blob:")
        || normalized.starts_with("javascript:")
    {
        return None;
    }

    Some(normalized)
}

fn looks_like_gif_image_src(src: &str) -> bool {
    let lower = src.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }

    lower.starts_with("data:image/gif")
        || lower.contains(".gif")
        || lower.contains("format=gif")
        || lower.contains("fm=gif")
        || lower.contains("mime=image/gif")
        || lower.contains("image/gif")
}

fn resolve_local_image_src_path(src: &str) -> Option<std::path::PathBuf> {
    let is_local = src.starts_with("file://")
        || (src.len() > 2
            && src.chars().nth(1) == Some(':')
            && (src.chars().nth(2) == Some('\\') || src.chars().nth(2) == Some('/')));
    if !is_local {
        return None;
    }

    let path_str = if src.starts_with("file://") {
        let raw_path = src.trim_start_matches("file://");
        if raw_path.starts_with('/') && raw_path.chars().nth(2) == Some(':') {
            &raw_path[1..]
        } else {
            raw_path
        }
    } else {
        src
    };

    let decoded_path = decode(path_str)
        .map(|p| p.into_owned())
        .unwrap_or(path_str.to_string());
    let clean_path = decoded_path
        .split('?')
        .next()
        .unwrap_or(&decoded_path)
        .split('#')
        .next()
        .unwrap_or(&decoded_path);
    let path = std::path::Path::new(clean_path);
    if !path.exists() {
        return None;
    }

    Some(path.to_path_buf())
}

fn gif_data_url_from_bytes(bytes: &[u8]) -> Option<String> {
    let ext = image_ext_from_bytes(bytes).or_else(|| {
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("gif")
        } else {
            None
        }
    })?;
    if ext != "gif" {
        return None;
    }

    let b64 = general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{};base64,{}", image_mime_by_ext(ext), b64))
}

fn image_data_url_from_bytes(bytes: &[u8]) -> Option<String> {
    let ext = image_ext_from_bytes(bytes).or_else(|| {
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("gif")
        } else {
            None
        }
    })?;

    let b64 = general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{};base64,{}", image_mime_by_ext(ext), b64))
}

fn resolve_image_src_to_data_url(src: &str) -> Option<String> {
    let value = src.trim();
    if value.starts_with("data:image/") {
        return Some(value.to_string());
    }

    if let Some(path) = resolve_local_image_src_path(value) {
        let bytes = std::fs::read(&path).ok()?;
        return image_data_url_from_bytes(&bytes);
    }

    None
}

fn resolve_animated_image_src_to_data_url(src: &str) -> Option<String> {
    let value = src.trim();
    if value.starts_with("data:image/gif") {
        return Some(value.to_string());
    }

    if !looks_like_gif_image_src(value) {
        return None;
    }

    if let Some(path) = resolve_local_image_src_path(value) {
        let bytes = std::fs::read(&path).ok()?;
        return gif_data_url_from_bytes(&bytes);
    }

    if let Some(remote_url) = normalize_remote_img_url(value) {
        let (bytes, ext) = fetch_remote_image(&remote_url)?;
        if ext == "gif" {
            return gif_data_url_from_bytes(&bytes);
        }
    }

    None
}

pub fn extract_animated_image_data_url_from_html(html: &str) -> Option<String> {
    if html.trim().is_empty() {
        return None;
    }

    static IMG_TAG_RE: OnceLock<Regex> = OnceLock::new();
    static IMG_ATTR_RE: OnceLock<Regex> = OnceLock::new();

    let img_tag_re = IMG_TAG_RE.get_or_init(|| Regex::new(r"(?is)<img\b[^>]*>").unwrap());
    let img_attr_re = IMG_ATTR_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)(src|data-src|data-original|data-actualsrc|srcset)\s*=\s*["']([^"']+)["']"#,
        )
        .unwrap()
    });

    for tag in img_tag_re.find_iter(html) {
        for caps in img_attr_re.captures_iter(tag.as_str()) {
            let Some(raw_src) = caps.get(2).map(|m| m.as_str()) else {
                continue;
            };
            let Some(candidate) = normalize_html_image_src_candidate(raw_src) else {
                continue;
            };
            if let Some(data_url) = resolve_animated_image_src_to_data_url(&candidate) {
                return Some(data_url);
            }
        }
    }

    None
}

pub fn extract_first_image_data_url_from_html(html: &str) -> Option<String> {
    if html.trim().is_empty() {
        return None;
    }

    static IMG_TAG_RE: OnceLock<Regex> = OnceLock::new();
    static IMG_ATTR_RE: OnceLock<Regex> = OnceLock::new();

    let img_tag_re = IMG_TAG_RE.get_or_init(|| Regex::new(r"(?is)<img\b[^>]*>").unwrap());
    let img_attr_re = IMG_ATTR_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)(src|data-src|data-original|data-actualsrc|srcset)\s*=\s*["']([^"']+)["']"#,
        )
        .unwrap()
    });

    for tag in img_tag_re.find_iter(html) {
        for caps in img_attr_re.captures_iter(tag.as_str()) {
            let Some(raw_src) = caps.get(2).map(|m| m.as_str()) else {
                continue;
            };
            let Some(candidate) = normalize_html_image_src_candidate(raw_src) else {
                continue;
            };
            if let Some(data_url) = resolve_image_src_to_data_url(&candidate) {
                return Some(data_url);
            }
        }
    }

    None
}

pub fn extract_animated_image_data_url_from_text(text: &str) -> Option<String> {
    let candidate = normalize_html_image_src_candidate(text)?;
    resolve_animated_image_src_to_data_url(&candidate)
}

fn save_image_bytes_to_attachments(
    bytes: &[u8],
    ext: &str,
    attachments_dir: &Path,
) -> Option<String> {
    let ext = normalize_image_ext(ext)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    bytes.hash(&mut hasher);
    let hash = hasher.finish();

    let file_name = format!("img_{:x}.{}", hash, ext);
    let target = attachments_dir.join(file_name);
    if !target.exists() {
        std::fs::write(&target, bytes).ok()?;
    }
    let path = target.to_string_lossy().replace('\\', "/");
    if path.starts_with('/') {
        Some(format!("file://{}", path))
    } else {
        Some(format!("file:///{}", path))
    }
}

/// 去掉全部空白（含换行）。用于"只差空白"的比对。
fn strip_all_whitespace(text: &str) -> String {
    static WS_RE: OnceLock<Regex> = OnceLock::new();
    WS_RE
        .get_or_init(|| Regex::new(r"\s+").unwrap())
        .replace_all(text, "")
        .to_string()
}

fn collapse_preview_whitespace(text: &str) -> String {
    static WHITESPACE_RE: OnceLock<Regex> = OnceLock::new();

    let normalized = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', " ");
    WHITESPACE_RE
        .get_or_init(|| Regex::new(r"\s+").unwrap())
        .replace_all(&normalized, " ")
        .trim()
        .to_string()
}

pub fn build_clipboard_text_fingerprint(
    content_type: &str,
    content: &str,
    html_content: Option<&str>,
) -> String {
    match content_type {
        "rich_text" => {
            collapse_preview_whitespace(&derive_rich_text_content(content, html_content))
        }
        "text" | "code" | "url" => {
            collapse_preview_whitespace(&normalize_clipboard_plain_text(content))
        }
        _ => String::new(),
    }
}

fn collapse_line_whitespace(text: &str) -> String {
    static WHITESPACE_RE: OnceLock<Regex> = OnceLock::new();

    WHITESPACE_RE
        .get_or_init(|| Regex::new(r"[^\S\r\n]+").unwrap())
        .replace_all(text.trim(), " ")
        .trim()
        .to_string()
}

fn normalize_plain_text_layout(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = Vec::new();

    for raw_line in normalized.lines() {
        let line = collapse_line_whitespace(raw_line);
        if line.is_empty() {
            if !lines
                .last()
                .map(|last: &String| last.is_empty())
                .unwrap_or(false)
            {
                lines.push(String::new());
            }
        } else {
            lines.push(line);
        }
    }

    let start = lines
        .iter()
        .position(|line| !line.is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map(|idx| idx + 1)
        .unwrap_or(start);

    lines[start..end].join("\n")
}

fn decode_basic_html_entities(text: &str) -> String {
    // Kept as the original sequential replacement. It sits on the shared
    // HTML→text path, and its double-decoding behaviour (e.g. `&amp;lt;` → `<`)
    // is relied upon by existing preview output, so it is deliberately not
    // "improved" here.
    text.replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn is_office_style_definition_text(text: &str) -> bool {
    static OFFICE_STYLE_SIGNAL_RE: OnceLock<Regex> = OnceLock::new();

    let normalized = collapse_preview_whitespace(text);
    normalized.len() > 24
        && OFFICE_STYLE_SIGNAL_RE
            .get_or_init(|| {
                Regex::new(
                    r"(?is)(/\*\s*style definitions\s*\*/|mso-style-name|mso-style-noshow|mso-style-priority|mso-padding-alt|mso-para-margin|table\.mso|mso-|microsoftinternetexplorer\d*|documentnotspecified|wps office|office word|msonormal|mso normal|normal\s+\d+\s+false)"
                )
                .unwrap()
            })
            .is_match(&normalized)
}

fn strip_leading_office_metadata_text(text: &str) -> String {
    static OFFICE_METADATA_PREFIX_RE: OnceLock<Regex> = OnceLock::new();

    let normalized = normalize_plain_text_layout(text);
    if normalized.is_empty() {
        return normalized;
    }

    // Strip CF_HTML header if present in this context
    let stripped_header = normalize_clipboard_plain_text(&normalized);
    if stripped_header.is_empty() {
        return String::new();
    }

    let lower = stripped_header.to_ascii_lowercase();
    if !(lower.contains("microsoftinternetexplorer") || lower.contains("documentnotspecified")) {
        return stripped_header;
    }

    let stripped = OFFICE_METADATA_PREFIX_RE
        .get_or_init(|| {
            Regex::new(
                r"(?is)^\s*(?:(?:\d+|false|true|[a-z]{2}(?:-[a-z]{2})?|x-none|normal|documentnotspecified|microsoftinternetexplorer\d*|[\d.]+(?:pt|px|磅))\s+)+"
            )
            .unwrap()
        })
        .replace(&stripped_header, "")
        .trim()
        .to_string();

    if stripped.is_empty() {
        stripped_header
    } else {
        stripped
    }
}

fn extract_renderable_html_region(html: &str) -> String {
    static BODY_RE: OnceLock<Regex> = OnceLock::new();
    static HEAD_RE: OnceLock<Regex> = OnceLock::new();

    let repaired = repair_html_fragment(html);
    let trimmed = repaired.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(start_idx) = trimmed.find("<!--StartFragment-->") {
        let start = start_idx + "<!--StartFragment-->".len();
        if let Some(end_rel) = trimmed[start..].find("<!--EndFragment-->") {
            return trimmed[start..start + end_rel].trim().to_string();
        }
    }

    // Fallback: If it has CF_HTML header but no markers, try to strip the header
    if looks_like_cf_html_header_text(trimmed) {
        let stripped = normalize_clipboard_plain_text(trimmed);
        if !stripped.is_empty() && stripped.len() < trimmed.len() {
            return stripped;
        }
    }

    if let Some(captures) = BODY_RE
        .get_or_init(|| Regex::new(r"(?is)<body\b[^>]*>([\s\S]*?)</body\s*>").unwrap())
        .captures(trimmed)
    {
        if let Some(body) = captures.get(1) {
            return body.as_str().trim().to_string();
        }
    }

    HEAD_RE
        .get_or_init(|| Regex::new(r"(?is)<head\b[\s\S]*?</head\s*>").unwrap())
        .replace_all(trimmed, " ")
        .trim()
        .to_string()
}

pub fn repair_html_fragment(html: &str) -> String {
    static MISSING_LEADING_TAG_RE: OnceLock<Regex> = OnceLock::new();

    let trimmed = html.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return trimmed.to_string();
    }

    let tag_like = MISSING_LEADING_TAG_RE
        .get_or_init(|| {
            Regex::new(
                r"(?is)^(table|tbody|thead|tfoot|tr|td|th|colgroup|col|div|span|p|ul|ol|li|blockquote|pre|h[1-6]|meta|style|img|a)\b[^>]*>"
            )
            .unwrap()
        })
        .is_match(trimmed);

    if tag_like {
        format!("<{}", trimmed)
    } else {
        trimmed.to_string()
    }
}

fn strip_office_preview_noise(text: &str) -> String {
    static OFFICE_STYLE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();
    static OFFICE_XML_BLOCK_RE: OnceLock<Regex> = OnceLock::new();
    static CONDITIONAL_COMMENT_RE: OnceLock<Regex> = OnceLock::new();
    static RENDERABLE_CONTENT_TAG_RE: OnceLock<Regex> = OnceLock::new();

    let mut processed = extract_renderable_html_region(text);
    if processed.trim().is_empty() {
        return processed.trim().to_string();
    }

    processed = OFFICE_XML_BLOCK_RE
        .get_or_init(|| Regex::new(r"(?is)<xml\b[\s\S]*?</xml>").unwrap())
        .replace_all(&processed, |caps: &regex::Captures| {
            let block = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            if is_office_style_definition_text(block) {
                " ".to_string()
            } else {
                block.to_string()
            }
        })
        .into_owned();

    processed = OFFICE_STYLE_BLOCK_RE
        .get_or_init(|| Regex::new(r"(?is)<style\b[\s\S]*?</style>").unwrap())
        .replace_all(&processed, |caps: &regex::Captures| {
            let block = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            if is_office_style_definition_text(block) {
                " ".to_string()
            } else {
                block.to_string()
            }
        })
        .into_owned();

    processed = CONDITIONAL_COMMENT_RE
        .get_or_init(|| Regex::new(r"(?is)<!--[\s\S]*?-->").unwrap())
        .replace_all(&processed, |caps: &regex::Captures| {
            let block = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            if is_office_style_definition_text(block) {
                " ".to_string()
            } else {
                block.to_string()
            }
        })
        .into_owned();

    if let Some(renderable_match) = RENDERABLE_CONTENT_TAG_RE
        .get_or_init(|| {
            Regex::new(r"(?is)<(table|p|div|span|img|a|ul|ol|li|blockquote|pre|h[1-6])\b").unwrap()
        })
        .find(&processed)
    {
        let prefix = &processed[..renderable_match.start()];
        if is_office_style_definition_text(prefix) {
            processed = processed[renderable_match.start()..].to_string();
        }
    }

    processed.trim().to_string()
}

fn looks_like_html_fragment_shallow(text: &str) -> bool {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('<') {
        return true;
    }

    if looks_like_cf_html_header_text(trimmed) {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    [
        "table ", "tbody", "thead", "tfoot", "tr ", "td ", "th ", "col ", "colgroup", "div ",
        "span ", "p ", "meta ", "style ",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        || (lower.contains("cellpadding=") && lower.contains("cellspacing="))
}

fn looks_like_html_fragment(text: &str) -> bool {
    let repaired = strip_office_preview_noise(text);
    looks_like_html_fragment_shallow(&repaired)
}

fn sanitize_rich_text_plain_text(text: &str) -> String {
    let normalized = normalize_plain_text_layout(text);
    if normalized.is_empty() {
        return normalized;
    }

    let stripped = strip_leading_office_metadata_text(&normalized);
    if is_office_style_definition_text(&collapse_preview_whitespace(&stripped)) {
        String::new()
    } else {
        stripped
    }
}

fn extract_plain_text_from_htmlish(text: &str) -> String {
    static BREAK_TAG_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();

    let repaired = strip_office_preview_noise(text);
    if repaired.is_empty() {
        return String::new();
    }
    let with_breaks = BREAK_TAG_RE
        .get_or_init(|| {
            Regex::new(
                r"(?is)</?(?:br|p|div|li|tr|td|th|table|h[1-6]|section|article|ul|ol)\b[^>]*>",
            )
            .unwrap()
        })
        .replace_all(&repaired, "\n");
    let without_tags = TAG_RE
        .get_or_init(|| Regex::new(r"(?is)<[^>]+>").unwrap())
        .replace_all(with_breaks.as_ref(), " ");
    let collapsed = normalize_plain_text_layout(&decode_basic_html_entities(without_tags.as_ref()));
    let cleaned = strip_leading_office_metadata_text(&collapsed);
    if cleaned.is_empty() {
        return String::new();
    }
    if is_office_style_definition_text(&collapse_preview_whitespace(&cleaned)) {
        String::new()
    } else {
        cleaned
    }
}

fn looks_like_obsidian_callout_markdown(text: &str) -> bool {
    static OBSIDIAN_CALLOUT_RE: OnceLock<Regex> = OnceLock::new();

    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let first_non_empty = normalized
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");

    OBSIDIAN_CALLOUT_RE
        .get_or_init(|| Regex::new(r"(?i)^>\s*\[\![a-z0-9_-]+\](?:[+-])?(?:\s+.+)?$").unwrap())
        .is_match(first_non_empty)
}

fn source_app_likely_formats_rich_text(source_app: &str, source_app_path: Option<&str>) -> bool {
    let mut haystack = source_app.to_ascii_lowercase();
    if let Some(path) = source_app_path {
        if !haystack.is_empty() {
            haystack.push(' ');
        }
        haystack.push_str(&path.to_ascii_lowercase());
    }

    [
        "wps",
        "winword",
        "word",
        "excel",
        "powerpoint",
        "onenote",
        "outlook",
        "soffice",
        "libreoffice",
        "writer",
        "calc",
        "impress",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

fn plain_text_has_rich_html_signals(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();

    lower.contains("<!--startfragment-->")
        || lower.contains("<!--endfragment-->")
        || lower.contains("<html")
        || lower.contains("<body")
        || lower.contains("<meta")
        || lower.contains("<style")
        || lower.contains("mso-")
        || lower.contains("documentnotspecified")
        || lower.contains("microsoftinternetexplorer")
        || lower.contains("class=mso")
        || lower.contains("class=\"mso")
        || lower.contains("cellpadding=")
        || lower.contains("cellspacing=")
}

pub fn looks_like_cf_html_header_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("version:0.9")
        || (lower.contains("starthtml:") && lower.contains("startfragment:"))
}

pub fn normalize_clipboard_plain_text(text: &str) -> String {
    static INLINE_CF_HTML_HEADER_RE: OnceLock<Regex> = OnceLock::new();

    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if !looks_like_cf_html_header_text(&normalized) {
        return normalized;
    }

    // Try parsing as CF_HTML first to get any legitimate fragments
    if let Some(html) = parse_cf_html(normalized.as_bytes()) {
        let plain = extract_plain_text_from_htmlish(&html);
        if !plain.trim().is_empty() {
            return plain;
        }
    }

    // Aggressively strip header metadata lines if present
    let mut lines = normalized.lines();
    let mut cleaned_lines = Vec::new();
    let mut in_header = true;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if in_header {
            let lower = trimmed.to_lowercase();
            let is_header_key = lower.starts_with("version:")
                || lower.starts_with("starthtml:")
                || lower.starts_with("endhtml:")
                || lower.starts_with("startfragment:")
                || lower.starts_with("endfragment:")
                || lower.starts_with("sourceurl:");

            if is_header_key || trimmed.is_empty() {
                continue;
            }
            // First line that doesn't look like a header key ends the header
            in_header = false;
        }
        cleaned_lines.push(line);
    }

    let result = cleaned_lines.join("\n").trim().to_string();
    if !result.is_empty() && result != normalized {
        if looks_like_html_fragment(&result) {
            let plain = extract_plain_text_from_htmlish(&result);
            if !plain.trim().is_empty() {
                return plain;
            }
        }
        return result;
    }

    let stripped_inline = INLINE_CF_HTML_HEADER_RE
        .get_or_init(|| {
            Regex::new(
                r"(?is)\b(?:version:\s*[^\s]+|starthtml:\s*\d+|endhtml:\s*\d+|startfragment:\s*\d+|endfragment:\s*\d+|sourceurl:\s*\S+)",
            )
            .unwrap()
        })
        .replace_all(&normalized, " ");
    let inline_result = normalize_plain_text_layout(stripped_inline.as_ref())
        .trim()
        .to_string();

    if inline_result.is_empty() {
        return normalized;
    }

    if looks_like_html_fragment(&inline_result) {
        let plain = extract_plain_text_from_htmlish(&inline_result);
        if !plain.trim().is_empty() {
            return plain;
        }
    }

    inline_result
}

pub fn infer_rich_html_from_plain_text(
    text: &str,
    source_app: &str,
    source_app_path: Option<&str>,
) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let html = parse_cf_html(trimmed.as_bytes())?;
    let plain_text = extract_plain_text_from_htmlish(&html);
    if plain_text.is_empty() {
        return None;
    }

    let normalized_source = collapse_preview_whitespace(trimmed);
    let normalized_plain = collapse_preview_whitespace(&plain_text);
    let materially_differs = normalized_source != normalized_plain;

    if plain_text_has_rich_html_signals(trimmed)
        || (source_app_likely_formats_rich_text(source_app, source_app_path) && materially_differs)
    {
        return Some(html);
    }

    None
}

/// Upper bound for treating a single token as a copied link. Anything longer is
/// page content rather than a URL, so the HTML text keeps winning there.
const BARE_URL_TEXT_MAX_CHARS: usize = 8192;

/// Is this text one bare URL and nothing else?
fn looks_like_bare_url_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > BARE_URL_TEXT_MAX_CHARS {
        return false;
    }
    if trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let Some(host) = url_host_of(trimmed) else {
        return false;
    };

    // Reject tokens that merely look host-shaped so that version numbers and
    // filenames ("3.14", "1.2.3", "readme.md") are not taken for links. Without a
    // scheme, a real link ends in an alphabetic top-level label; and a bare
    // "a.b"-style token with no path is far more likely a filename fragment.
    let has_scheme = trimmed.starts_with("http://") || trimmed.starts_with("https://");
    if !has_scheme && host != "localhost" {
        let tld = host.rsplit('.').next().unwrap_or_default();
        if tld.is_empty() || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
            return false;
        }
        if !trimmed.contains('/') && !trimmed.starts_with("www.") {
            return false;
        }
    }
    true
}

/// Registered host of a URL-shaped token (scheme optional), lowercased and with
/// userinfo, port and IPv6 brackets removed. `None` means the token is not
/// URL-shaped at all.
fn url_host_of(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let without_scheme = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .unwrap_or(trimmed);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return None;
    }

    // Drop a "user:password@" prefix, then a ":port" / "[v6]:port" suffix.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    }
    .to_ascii_lowercase();

    if host.is_empty() || host.len() > 253 {
        return None;
    }

    let well_formed = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    if !well_formed || (!host.contains('.') && host != "localhost") {
        return None;
    }

    Some(host)
}

/// Host comparison key for deciding whether two links point at the same site.
///
/// Comparing hosts literally is too strict: the same page is commonly reachable as
/// `example.com`, `www.example.com` and `docs.example.com`, and a copied link whose
/// anchor uses a sibling subdomain is still the same site. So subdomains are
/// collapsed to their registrable domain. A two-label suffix is treated as the
/// domain boundary, which is right for the common `example.com` case and errs
/// toward matching for longer public suffixes (never toward rejecting a real link).
fn comparable_url_host(host: &str) -> String {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return host.to_string();
    }
    labels[labels.len() - 2..].join(".")
}

/// First `href` value of an anchor in this HTML, entity-decoded and trimmed.
fn first_anchor_href(html: &str) -> Option<String> {
    static ANCHOR_HREF_RE: OnceLock<Regex> = OnceLock::new();

    let captures = ANCHOR_HREF_RE
        .get_or_init(|| {
            Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#).unwrap()
        })
        .captures(html)?;
    let raw = captures
        .get(1)
        .or_else(|| captures.get(2))
        .or_else(|| captures.get(3))?
        .as_str();
    let decoded = decode_basic_html_entities(raw).trim().to_string();
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

/// Does this payload describe a link whose plain text is the URL while the HTML
/// carries a text label for it?
///
/// Browsers write the anchor label into `CF_UNICODETEXT` and put the real
/// target only into `CF_HTML` (`<a href="…">label</a>`). The HTML text wins by
/// default, which turns "copy link" into "copy the page title": the URL never
/// reaches the database, and pasting — plain text included — yields the title
/// only. When the plain text really is the URL of the same site the anchor
/// points at, the URL is the more faithful content and is kept instead. A
/// different host means the HTML text is the genuine content (an in-article
/// hyperlink inside copied text), so the default preference stays untouched.
fn describes_href_backed_link(plain_text: &str, html: &str) -> bool {
    if !looks_like_bare_url_text(plain_text) {
        return false;
    }
    let Some(href) = first_anchor_href(html) else {
        return false;
    };
    let (Some(plain_host), Some(href_host)) = (url_host_of(plain_text), url_host_of(&href)) else {
        return false;
    };
    if comparable_url_host(&plain_host) != comparable_url_host(&href_host) {
        return false;
    }

    // The label must actually differ from the URL; otherwise the default path
    // already produces the same value.
    let html_text = extract_plain_text_from_htmlish(html);
    collapse_preview_whitespace(&html_text) != collapse_preview_whitespace(plain_text)
}

/// HTML 里的可见文字**整体就是一个链接**时，返回那个链接的 href。
///
/// # 为什么需要它
///
/// 从网页/内网 wiki 复制一条链接时，剪贴板里的 HTML 往往是
/// `<a href="http://host/c/T750/+/176116">点击查看</a>`：屏幕上显示的是带下划线的
/// 标签文字，而链接地址藏在 `href` 里。纯文本正文代表"以纯文本形式粘贴出去会是
/// 什么"，此时用户要的是**链接本身**（能点、能复制、能直接打开），不是那层标签。
///
/// # 为什么不能无条件替换所有 `<a>`
///
/// 文章正文里的行内链接必须保留原文：`<p>Read the <a href="...">full story</a></p>`
/// 粘贴成纯文本应当是 "Read the full story"，而不是把句子中间插进一个网址。所以
/// 只在**锚点就是全部可见内容**时才替换。判断方式是拿 HTML 的可见文字与锚点文字的
/// 归一化结果做比较，而不是数标签 —— 后者会被 `<p>`、`<div>` 这类包裹层干扰。
///
/// 多链接、嵌套链接、href 为空或指向 `javascript:` / `data:` 的一律不替换。
/// 富文本条目的**纯文本正文**（已按"所有超链接换成网址本身"的口径派生）。
///
/// # 为什么要有一个统一入口
///
/// 读取侧过去各自决定要不要调用派生函数，于是漏掉的地方会把链接标签直接交给用户
/// 或下游：顺序粘贴、MCP 暴露给 AI、云同步搬运都曾如此。同一个规则散落成多份判断，
/// 就一定会漏。
///
/// 非富文本条目原样返回（除非命中下面的存量修复）。
pub fn plain_text_of_entry(item: &crate::domain::models::ClipboardEntry) -> String {
    plain_text_of(
        &item.content,
        &item.content_type,
        item.html_content.as_deref(),
    )
}

/// 把条目的正文就地归一化成对外形态（链接是地址）。用于事件载荷等"直接送实体给
/// 界面"的通道 —— 那些通道绕过了列表命令的归一化，是本规则最容易漏的一类出口。
pub fn normalize_content_for_ui(item: &mut crate::domain::models::ClipboardEntry) {
    let plain = plain_text_of_entry(item);
    if !plain.trim().is_empty() {
        item.content = plain;
    }
}

/// [`plain_text_of_entry`] 的底层形式，供手上只有三个字段、没有整条 `ClipboardEntry`
/// 的调用点使用（复制、粘贴队列等）。
pub fn plain_text_of(content: &str, content_type: &str, html_content: Option<&str>) -> String {
    if content_type == "rich_text" {
        let derived = derive_rich_text_content(content, html_content);
        if !derived.trim().is_empty() {
            return derived;
        }
        return content.to_string();
    }

    // 已被"转换为纯文本"降级过的存量行。
    //
    // 转换会把 `content_type` 改成 `text`（`html_content` 保留原样），此后
    // `rich_text` 的派生分支不再覆盖它，于是早先存进去的链接标签就永久留在了列表、
    // 粘贴与搜索里 —— 用户会以为"改了也没生效"。
    //
    // 但不能无脑用 HTML 覆盖：用户可能后来手改过正文，那份 HTML 已经是旧的。判定
    // 依据是**存储的正文是否恰好就是那份 HTML 的可见文字**（即早先口径的派生结果）：
    // 相等说明它只是陈旧派生，可以安全换成新口径；不等说明正文被改过，保持原样。
    if let Some(html) = html_content {
        if !html.trim().is_empty() {
            let legacy = extract_plain_text_from_htmlish(html);
            let derived = derive_rich_text_content(content, Some(html));
            if !legacy.trim().is_empty() && !derived.trim().is_empty() && derived != content {
                // 两种情况都算"这条正文只是当年转换留下的旧结果"：
                //
                // 1. 与旧口径的可见文字完全相同 —— 直接比对即成立。
                // 2. 去空白后与旧口径相同 —— 旧按钮读的是浏览器 `innerText`（链接紧贴
                //    正文时不补空格），而旧后端口径会在标签处补空格，于是
                //    `详见标签说明` 与 `详见 标签 说明` 只差空白。按空白归一后比对，
                //    这批"链接紧贴正文"的存量行才能一并救回来。
                //
                // 用户手写的内容不会同时满足这两条：它要么在文字上就不等于那份 HTML，
                // 要么长度/用词已经不同。
                let same_exact =
                    collapse_preview_whitespace(&legacy) == collapse_preview_whitespace(content);
                let same_ignoring_spaces =
                    strip_all_whitespace(&legacy) == strip_all_whitespace(content);
                if same_exact || same_ignoring_spaces {
                    return derived;
                }
            }
        }
    }

    content.to_string()
}

/// 把 HTML 里的每个 `<a href>` 换成 href 本身（返回改写后的 HTML）。
///
/// # 为什么是"每个"而不是"整条只有一个链接时"
///
/// 纯文本正文代表"这条内容以纯文本形式粘贴出去长什么样"。带下划线的标签文字是
/// 界面的装饰，链接的**真实目标**是 `href` —— 用户要能直接看到、复制、打开那个地址。
/// 所以只要有超链接，就换成网址本身，不论它在正文的什么位置、是不是唯一一个。
///
/// （早期的实现只在"整条内容恰好就是一个链接"时才替换，那会让正文里夹带的链接
/// 继续停留在标签文字上，用户反馈仍然看不到地址。）
///
/// # 不改写的情况
///
/// - 没有 href、href 为空；
/// - `javascript:` / `data:` / `vbscript:` 这类不是可导航目标；
/// - 标签文字与地址本来就完全相同（改写等于没改）。
///
/// 锚点内部的格式标签（`<a href="X"><b>粗</b></a>`）不用特殊处理：整个锚点被替换成
/// 地址后，原本嵌在里面的标签自然消失。
fn rewrite_anchor_hrefs_to_urls(html: &str) -> String {
    static ANCHOR_RE: OnceLock<Regex> = OnceLock::new();

    let anchor_re = ANCHOR_RE.get_or_init(|| {
        // 三种写法都要认：双引号、单引号、**不加引号**。
        // 只认前两种会让 `<a href=http://h/1>看这里</a>` 抓不到，而前端的 DOM 解析
        // 能认出它 —— 同一个文件用界面按钮和后端粘贴会得到不同结果。
        Regex::new(
            r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))[^>]*>.*?</a\s*>"#,
        )
        .unwrap()
    });

    anchor_re
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let href = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .map(|m| m.as_str().trim())
                .unwrap_or("");
            let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            if href.is_empty() || looks_like_non_navigable_href(href) {
                whole.to_string()
            } else {
                href.to_string()
            }
        })
        .into_owned()
}

/// `javascript:` / `data:` 这类不能当作链接目标回填到纯文本里的协议。
fn looks_like_non_navigable_href(href: &str) -> bool {
    static NON_NAVIGABLE_RE: OnceLock<Regex> = OnceLock::new();
    NON_NAVIGABLE_RE
        .get_or_init(|| Regex::new(r"(?i)^\s*(?:javascript|data|vbscript)\s*:").unwrap())
        .is_match(href)
}

pub fn derive_rich_text_content(content: &str, html_content: Option<&str>) -> String {
    let sanitized_plain = sanitize_rich_text_plain_text(content);
    if looks_like_obsidian_callout_markdown(&sanitized_plain) {
        return sanitized_plain;
    }

    // Keep the URL of an href-backed link instead of the anchor label offered by
    // the HTML. This is also what the paste paths see: they call this helper
    // again with the stored row, so a plain-text paste yields the URL as well.
    if let Some(html) = html_content {
        if describes_href_backed_link(&sanitized_plain, html) {
            return sanitized_plain;
        }
    }

    // 所有超链接一律换成网址本身，再做纯文本提取。
    // 富文本那条路径不受影响：它渲染的是 `html_content`，仍然显示带下划线的标签。
    let html_text = html_content
        .map(|html| extract_plain_text_from_htmlish(&rewrite_anchor_hrefs_to_urls(html)))
        .filter(|text| !text.is_empty());
    if let Some(text) = html_text {
        return text;
    }

    if looks_like_html_fragment(content) {
        let content_text = extract_plain_text_from_htmlish(content);
        if !content_text.is_empty() {
            return content_text;
        }
    }

    sanitized_plain
}

pub fn build_entry_preview(
    content_type: &str,
    content: &str,
    html_content: Option<&str>,
) -> String {
    if content_type == "image" {
        return "[Image Content]".to_string();
    }

    let preview_text = if content_type == "rich_text" {
        let clean_text = derive_rich_text_content(content, html_content);
        let preview = collapse_preview_whitespace(&clean_text);
        let normalized_content = collapse_preview_whitespace(content);

        if clean_text.is_empty()
            || preview.is_empty()
            || (html_content.is_none()
                && looks_like_html_fragment(content)
                && preview == normalized_content)
        {
            RICH_TEXT_PREVIEW_FALLBACK.to_string()
        } else {
            preview
        }
    } else {
        collapse_preview_whitespace(&normalize_clipboard_plain_text(content))
    };

    preview_text
}

pub fn attach_rich_image_fallback(html: &str, payload: &str) -> String {
    let mut out = String::with_capacity(
        html.len()
            + RICH_IMAGE_FALLBACK_PREFIX.len()
            + RICH_IMAGE_FALLBACK_SUFFIX.len()
            + payload.len()
            + 1,
    );
    out.push_str(html.trim_end());
    out.push('\n');
    out.push_str(RICH_IMAGE_FALLBACK_PREFIX);
    out.push_str(payload);
    out.push_str(RICH_IMAGE_FALLBACK_SUFFIX);
    out
}

pub fn split_rich_html_and_image_fallback(html: &str) -> (String, Option<String>) {
    if let Some(start) = html.rfind(RICH_IMAGE_FALLBACK_PREFIX) {
        let marker_start = start + RICH_IMAGE_FALLBACK_PREFIX.len();
        if let Some(end_rel) = html[marker_start..].find(RICH_IMAGE_FALLBACK_SUFFIX) {
            let marker_end = marker_start + end_rel;
            let mut cleaned = String::with_capacity(html.len());
            cleaned.push_str(&html[..start]);
            cleaned.push_str(&html[marker_end + RICH_IMAGE_FALLBACK_SUFFIX.len()..]);
            let payload = html[marker_start..marker_end].trim().to_string();
            return (cleaned.trim().to_string(), Some(payload));
        }
    }
    (html.to_string(), None)
}

pub fn attach_rich_named_formats(
    html: &str,
    formats: &[crate::infrastructure::windows_api::win_clipboard::NamedClipboardFormat],
) -> String {
    let stored: Vec<StoredNamedClipboardFormat> = formats
        .iter()
        .filter(|format| !format.name.trim().is_empty() && !format.data.is_empty())
        .map(|format| StoredNamedClipboardFormat {
            name: format.name.clone(),
            data_base64: general_purpose::STANDARD.encode(&format.data),
        })
        .collect();

    if stored.is_empty() {
        return html.to_string();
    }

    let Ok(payload_json) = serde_json::to_vec(&stored) else {
        return html.to_string();
    };

    let payload = general_purpose::STANDARD.encode(payload_json);
    let mut out = String::with_capacity(
        html.len()
            + RICH_NAMED_FORMATS_PREFIX.len()
            + RICH_NAMED_FORMATS_SUFFIX.len()
            + payload.len()
            + 1,
    );
    out.push_str(html.trim_end());
    out.push('\n');
    out.push_str(RICH_NAMED_FORMATS_PREFIX);
    out.push_str(&payload);
    out.push_str(RICH_NAMED_FORMATS_SUFFIX);
    out
}

pub fn split_rich_html_and_named_formats(
    html: &str,
) -> (
    String,
    Vec<crate::infrastructure::windows_api::win_clipboard::NamedClipboardFormat>,
) {
    if let Some(start) = html.rfind(RICH_NAMED_FORMATS_PREFIX) {
        let marker_start = start + RICH_NAMED_FORMATS_PREFIX.len();
        if let Some(end_rel) = html[marker_start..].find(RICH_NAMED_FORMATS_SUFFIX) {
            let marker_end = marker_start + end_rel;
            let payload = html[marker_start..marker_end].trim();

            let decoded = general_purpose::STANDARD.decode(payload);
            let parsed = decoded
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<StoredNamedClipboardFormat>>(&bytes).ok())
                .map(|items| {
                    items
                        .into_iter()
                        .filter_map(|item| {
                            let data = general_purpose::STANDARD.decode(item.data_base64).ok()?;
                            if item.name.trim().is_empty() || data.is_empty() {
                                return None;
                            }
                            Some(
                                crate::infrastructure::windows_api::win_clipboard::NamedClipboardFormat {
                                    name: item.name,
                                    data,
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                });

            if let Some(formats) = parsed {
                let mut cleaned = String::with_capacity(html.len());
                cleaned.push_str(&html[..start]);
                cleaned.push_str(&html[marker_end + RICH_NAMED_FORMATS_SUFFIX.len()..]);
                return (cleaned.trim().to_string(), formats);
            }
        }
    }
    (html.to_string(), Vec::new())
}

pub fn externalize_rich_image_fallback(html: &str, data_dir: &Path) -> String {
    let (clean_html, payload_opt) = split_rich_html_and_image_fallback(html);
    let Some(payload) = payload_opt else {
        return html.to_string();
    };

    if !payload.starts_with("data:image/") {
        return html.to_string();
    }

    if let Some(saved_path) = save_image_to_file(&payload, data_dir) {
        let base_html = if clean_html.trim().is_empty() {
            html
        } else {
            clean_html.as_str()
        };
        return attach_rich_image_fallback(base_html, &saved_path);
    }

    html.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        app_cleanup_policy_matches, apply_cleanup_rules, attach_rich_image_fallback,
        attach_rich_named_formats, build_entry_preview, collapse_preview_whitespace,
        decode_basic_html_entities, derive_rich_text_content,
        extract_animated_image_data_url_from_html,
        extract_animated_image_data_url_from_text, extract_first_image_data_url_from_html,
        infer_rich_html_from_plain_text,
        looks_like_bare_url_text, normalize_clipboard_plain_text, plain_text_of,
        plain_text_of_entry,
        parse_app_cleanup_policies, parse_cf_html, parse_cleanup_rules,
        split_rich_html_and_image_fallback, split_rich_html_and_named_formats,
        AppCleanupPolicy,
    };
    use base64::Engine;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_test_png_file(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("tiez_clip_utils_{}_{}", std::process::id(), unique));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4////fwAJ+wP9KobjigAAAABJRU5ErkJggg==")
            .unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn cleanup_test_path(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    fn file_url_for(path: &Path) -> String {
        let raw = path.to_string_lossy().replace('\\', "/");
        if raw.starts_with('/') {
            format!("file://{}", raw)
        } else {
            format!("file:///{}", raw)
        }
    }

    #[test]
    fn rich_text_preview_prefers_readable_html_text() {
        let html = "<table><tr><td>Alpha</td><td>Beta</td></tr><tr><td>Gamma</td><td>Delta</td></tr></table>";
        let preview = build_entry_preview("rich_text", "table border=0 cellpadding=0", Some(html));

        assert_eq!(preview, "Alpha Beta Gamma Delta");
    }

    #[test]
    fn rich_text_preview_hides_markup_only_plain_text() {
        let preview = build_entry_preview(
            "rich_text",
            "table border=0 cellpadding=0 cellspacing=0 width=288",
            None,
        );

        assert_eq!(preview, "[Rich Text Content]");
    }

    #[test]
    fn rich_text_preview_strips_office_style_definition_noise() {
        let html = concat!(
            "Normal 0 false false false EN-US ZH-CN X-NONE ",
            "/* Style Definitions */ ",
            "table.MsoNormalTable {mso-style-name:普通表格; mso-style-noshow:yes;} ",
            "<table><tr><td>学院意见</td><td>通过</td></tr></table>"
        );

        let preview = build_entry_preview("rich_text", html, Some(html));

        assert_eq!(preview, "学院意见 通过");
    }

    #[test]
    fn rich_text_content_prefers_renderable_html_over_wps_plain_text_noise() {
        let text =
            "1 1 1 1 MicrosoftInternetExplorer4 0 2 DocumentNotSpecified 7.8 磅 Normal 0 顶顶顶顶";
        let html = "<html><head><meta charset=\"utf-8\"><style>body{font-family:\"Times New Roman\";}</style></head><body><p>顶顶顶顶</p></body></html>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, "顶顶顶顶");
    }

    #[test]
    fn rich_text_content_keeps_copied_link_url_instead_of_anchor_label() {
        // Browser "copy link": plain text is the URL, HTML only carries the label.
        let text = "https://example.com/article";
        let html = "<a href=\"https://example.com/article\">Example Site | Home</a>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, text);
    }

    #[test]
    fn rich_text_content_keeps_copied_link_url_without_scheme_or_www_prefix() {
        // Same entry re-derived on the paste path, where the stored plain text
        // may already have lost its scheme while the href kept it.
        let text = "www.example.com/article";
        let html = "<a href=\"http://example.com/article\">Example Site | Home</a>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, text);
    }

    /// 用户报的场景：内网 wiki 的一条链接，屏幕上显示的是标签文字（带下划线），
    /// 地址藏在 href 里。纯文本正文应当是**地址本身**。
    #[test]
    fn sole_link_content_yields_the_href_for_plain_text_paste() {
        let label = "T750 变更单";
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<a href=\"{url}\">{label}</a>");

        let content = derive_rich_text_content(label, Some(&html));

        assert_eq!(content, url);
        assert!(!content.contains(label), "纯文本正文不应只剩标签文字");
    }

    /// 同上，但链接被块级元素包着（从网页复制时的常见形态）。
    #[test]
    fn sole_link_wrapped_in_blocks_still_yields_the_href() {
        let label = "T750 变更单";
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<div><p><span><a href=\"{url}\">{label}</a></span></p></div>");

        assert_eq!(derive_rich_text_content(label, Some(&html)), url);
    }

    /// 统一入口：富文本条目取出纯文本正文时，链接必须是地址。
    ///
    /// 这条盯住的是"读取侧忘了派生"这类漏 —— 顺序粘贴、MCP、云同步都曾经直接把
    /// 库里的旧正文交出去。
    #[test]
    fn plain_text_of_entry_derives_the_url_for_rich_text() {
        let item = crate::domain::models::ClipboardEntry {
            id: 1,
            content_type: "rich_text".to_string(),
            content: "T750 变更单".to_string(),
            html_content: Some(
                "<p><a href=\"http://192.168.23.98:8888/c/T750/+/176116\">T750 变更单</a></p>"
                    .to_string(),
            ),
            source_app: "test".to_string(),
            timestamp: 0,
            preview: String::new(),
            is_pinned: false,
            pinned_order: 0,
            tags: Vec::new(),
            use_count: 0,
            note: String::new(),
            source_app_path: None,
            is_external: false,
            file_preview_exists: false,
        };

        assert_eq!(
            plain_text_of_entry(&item),
            "http://192.168.23.98:8888/c/T750/+/176116"
        );
    }

    /// 非富文本条目原样返回，不被改写。
    #[test]
    fn plain_text_of_entry_leaves_other_types_alone() {
        let item = crate::domain::models::ClipboardEntry {
            id: 2,
            content_type: "text".to_string(),
            content: "普通文字".to_string(),
            html_content: None,
            source_app: "test".to_string(),
            timestamp: 0,
            preview: String::new(),
            is_pinned: false,
            pinned_order: 0,
            tags: Vec::new(),
            use_count: 0,
            note: String::new(),
            source_app_path: None,
            is_external: false,
            file_preview_exists: false,
        };

        assert_eq!(plain_text_of_entry(&item), "普通文字");
    }

    /// 富文本但 HTML 缺失时，退回存储的正文，不要变成空串。
    #[test]
    fn plain_text_of_entry_falls_back_when_html_is_missing() {
        let item = crate::domain::models::ClipboardEntry {
            id: 3,
            content_type: "rich_text".to_string(),
            content: "有些文字".to_string(),
            html_content: None,
            source_app: "test".to_string(),
            timestamp: 0,
            preview: String::new(),
            is_pinned: false,
            pinned_order: 0,
            tags: Vec::new(),
            use_count: 0,
            note: String::new(),
            source_app_path: None,
            is_external: false,
            file_preview_exists: false,
        };

        assert_eq!(plain_text_of_entry(&item), "有些文字");
    }

    /// 存量行【链接紧贴正文】的恢复。
    ///
    /// 旧按钮读的是浏览器 `innerText`（紧贴时不补空格），旧后端口径却会在标签处补
    /// 空格，于是库里存的是 `详见标签说明`、而那份 HTML 的旧可见文字是 `详见 标签 说明`。
    /// 只做精确比对会漏掉这一批，用户升级后那条仍显示标签。
    #[test]
    fn glueed_link_stale_row_recovers_the_url() {
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<p>详见<a href=\"{url}\">标签</a>说明</p>");

        // 紧贴写法（innerText 的产物，无空格）：网址被正确取出，原文次序不变。
        // 断言只检查"网址在、链接标签不再作为链接出现"，不锁死空格 —— 那是
        // `extract_plain_text_from_htmlish` 既有的排版行为，不属于本次改动范围。
        let glued = plain_text_of("详见标签说明", "text", Some(&html));
        assert!(
            glued.contains(url),
            "紧贴写法也应取出网址，实际得到: {glued:?}"
        );
        assert!(
            glued.contains("详见") && glued.contains("说明"),
            "链接周围的文字必须保留，实际得到: {glued:?}"
        );

        // 带空格写法（旧后端口径）同样能恢复
        let spaced = plain_text_of("详见 标签 说明", "text", Some(&html));
        assert!(spaced.contains(url), "带空格的旧结果也应恢复: {spaced:?}");
    }

    /// 用户真正手写过的正文不能被"去掉空白也算相同"误伤。
    #[test]
    fn hand_written_text_is_not_overwritten_by_whitespace_tolerance() {
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<p>详见<a href=\"{url}\">标签</a>说明</p>");

        // 文字本身不同（多了一个字），即使去掉空白也不相等
        let kept = plain_text_of("详见标签说明补充", "text", Some(&html));

        assert_eq!(kept, "详见标签说明补充");
    }

    /// 不加引号的 href 也要认。
    ///
    /// 前端用 DOM 解析天然认得它；后端正则若只认两种引号，同一个文件用界面按钮
    /// 与后端粘贴会得到不同结果（一边是网址、一边是标签）。
    #[test]
    fn unquoted_href_is_recognised() {
        let content = derive_rich_text_content(
            "看这里",
            Some("<p><a href=http://h/1>看这里</a></p>"),
        );

        assert_eq!(content, "http://h/1");
    }

    /// 真实形态：从浏览器/内网 wiki 复制链接时，剪贴板 HTML 是带 Windows 剪贴板
    /// 标记（`StartFragment` / `EndFragment`）与 Office 噪声的完整文档，链接藏在
    /// 这些包裹层里面。用用户实际遇到的那条链接验证端到端结果。
    #[test]
    fn real_clipboard_html_with_fragment_markers_yields_the_url() {
        let html = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000300\r\n\
StartFragment:0000000141\r\nEndFragment:0000000264\r\n\
<html><body>\r\n<!--StartFragment--><p class=\"MsoNormal\"><a \
href=\"http://192.168.23.98:8888/c/T750/+/176116\">T750 变更单</a></p>\
<!--EndFragment-->\r\n</body></html>";

        let content = derive_rich_text_content("T750 变更单", Some(html));

        assert_eq!(content, "http://192.168.23.98:8888/c/T750/+/176116");
    }

    /// 富文本**渲染**用的 HTML 不能被改动：界面上仍要显示带下划线的标签。
    /// 只有"纯文本形态"才换成网址。
    #[test]
    fn html_itself_is_never_rewritten() {
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<p><a href=\"{url}\">T750 变更单</a></p>");

        let content = derive_rich_text_content("T750 变更单", Some(&html));

        assert_eq!(content, url, "纯文本形态是网址");
        assert!(html.contains("T750 变更单"), "原始 HTML 变量本身不变");
        assert!(html.contains(&format!("href=\"{url}\"")));
    }

    /// 存量修复：已降级成 `text`、正文里存着链接标签的行，读取时给出地址。
    ///
    /// 这正是用户"改了还是不对"的那批数据：早先转换时把界面上的标签文字存进了
    /// `content`，而 `html_content`（含真实地址）保留着。转换时类型已降级为 `text`，
    /// 从此 `rich_text` 的派生分支不再覆盖它。
    #[test]
    fn stale_converted_row_recovers_the_url() {
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<p><a href=\"{url}\">T750 变更单</a></p>");

        let recovered = plain_text_of("T750 变更单", "text", Some(&html));

        assert_eq!(recovered, url);
    }

    /// 但**不能**覆盖用户手改过的正文。
    ///
    /// 判定依据是"存储的正文是否恰好就是那份 HTML 的可见文字"：不等就说明正文被人
    /// 改过，那份 HTML 已经过期，必须保持原样。
    #[test]
    fn stale_recovery_leaves_hand_edited_content_alone() {
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<p><a href=\"{url}\">T750 变更单</a></p>");

        let kept = plain_text_of("我自己改写的说明", "text", Some(&html));

        assert_eq!(kept, "我自己改写的说明");
    }

    /// 普通 `text` 条目（没有 HTML）不受影响。
    #[test]
    fn stale_recovery_ignores_rows_without_html() {
        assert_eq!(plain_text_of("普通文字", "text", None), "普通文字");
        assert_eq!(plain_text_of("普通文字", "text", Some("")), "普通文字");
    }

    /// 标签与地址本来就相同、没有需要修复的内容时不改动。
    #[test]
    fn stale_recovery_is_a_noop_when_nothing_to_fix() {
        let url = "http://192.168.23.98:8888/c/T750/+/176116";
        let html = format!("<p><a href=\"{url}\">{url}</a></p>");

        assert_eq!(plain_text_of(url, "text", Some(&html)), url);
    }

    /// 行内链接也换成地址，周围文字保留。
    ///
    /// 这条测试原先断言"行内链接保留人话标签"。用户明确要求改成**所有**超链接都
    /// 换成网址本身（"我要的是超链接转换为链接网址本身而不是网址标题"），所以期望值
    /// 翻转 —— 这是需求变更，不是回归。
    #[test]
    fn inline_link_inside_prose_also_becomes_the_url() {
        let text = "详见 T750 变更单里的说明";
        let html = "<p>详见 <a href=\"http://192.168.23.98:8888/c/T750/+/176116\">T750 变更单</a>里的说明</p>";

        let content = derive_rich_text_content(text, Some(html));

        assert!(
            content.contains("http://192.168.23.98:8888/c/T750/+/176116"),
            "行内链接应当换成网址本身，实际得到: {content:?}"
        );
        assert!(
            content.contains("详见") && content.contains("里的说明"),
            "链接周围的文字必须保留，实际得到: {content:?}"
        );
    }

    /// 多个链接**各自**换成自己的地址（不是挑一个，也不是都不换）。
    #[test]
    fn every_link_becomes_its_own_url() {
        let text = "变更单 与 版本说明";
        let html = "<p><a href=\"http://a.example/1\">变更单</a> 与 <a href=\"http://b.example/2\">版本说明</a></p>";

        let content = derive_rich_text_content(text, Some(html));

        assert!(content.contains("http://a.example/1"), "第一条链接应换成地址: {content:?}");
        assert!(content.contains("http://b.example/2"), "第二条链接应换成地址: {content:?}");
    }

    /// 反向：`javascript:` 不是可导航地址，保留原文。
    #[test]
    fn non_navigable_href_is_not_used_as_plain_text() {
        let label = "点我";
        let html = "<a href=\"javascript:void(0)\">点我</a>";

        assert_eq!(derive_rich_text_content(label, Some(html)), label);
    }

    /// 文章正文里的链接同样换成地址（期望值按新契约翻转）。
    #[test]
    fn in_article_link_also_becomes_the_url() {
        let text = "Read the full story";
        let html = "<p>Read the <a href=\"https://example.com/article\">full story</a></p>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, "Read the https://example.com/article");
    }

    /// 锚点指向别处时，以锚点的地址为准（它才是这份 HTML 里真正的链接目标）。
    ///
    /// 期望值由"保留可见文字里的那个 URL"翻转为 href —— 与"所有超链接换成网址本身"
    /// 的新契约一致。
    #[test]
    fn anchor_target_wins_when_it_points_to_another_host() {
        let text = "https://example.com/article";
        let html = "<p><a href=\"https://other.example.org/other\">https://example.com/article</a></p>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, "https://other.example.org/other");
    }

    /// 整条内容就是一个带下划线的链接时，纯文本正文取**地址**。
    ///
    /// 这条测试原本断言保留标签文字（"Example Site | Home"）。用户明确要求改掉：
    /// 富文本显示的是下划线标签，而以纯文本粘贴/转换时应当得到链接本身。所以期望值
    /// 由标签翻转为 href —— 这是本次需求的核心行为，不是回归。
    #[test]
    fn anchor_labelled_link_yields_the_url_when_plain_text_is_a_label() {
        let text = "Example Site | Home";
        let html = "<a href=\"https://example.com/article\">Example Site | Home</a>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, "https://example.com/article");
    }

    #[test]
    fn rich_text_content_keeps_url_when_anchor_uses_a_sibling_subdomain() {
        // The same site is routinely reachable as example.com, www.example.com and
        // docs.example.com. A copied link whose anchor uses a sibling subdomain must
        // still be recognised as the same site, otherwise the URL is replaced by the
        // page title — the exact symptom being fixed.
        let text = "https://example.com/guide";
        let html = "<a href=\"https://docs.example.com/guide\">Example Guide</a>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, "https://example.com/guide");
    }

    #[test]
    fn bare_url_detection_rejects_version_strings_and_filenames() {
        // These are host-shaped but are not links; treating them as links would let a
        // stray anchor in the HTML replace genuine text content.
        for token in ["3.14", "1.2.3", "readme.md", "main.rs", "a.b", "192.168.1.1"] {
            assert!(!looks_like_bare_url_text(token), "不应视为链接: {token}");
        }
    }

    #[test]
    fn bare_url_detection_accepts_real_links() {
        for token in [
            "https://example.com/a",
            "http://example.com",
            "www.example.com/page",
            "example.com/page",
            "localhost:8080/x",
        ] {
            assert!(looks_like_bare_url_text(token), "应视为链接: {token}");
        }
    }

    #[test]
    fn html_entity_decoding_keeps_sequential_replacement_semantics() {
        // This function sits on the shared HTML→text path and its double-decoding
        // behaviour is relied on by existing preview output. Locking it here so a
        // future "cleanup" cannot silently change visible text.
        assert_eq!(decode_basic_html_entities("a &amp; b"), "a & b");
        assert_eq!(decode_basic_html_entities("x&nbsp;y"), "x y");
        assert_eq!(decode_basic_html_entities("&#34;q&#34;"), "\"q\"");
        assert_eq!(decode_basic_html_entities("&amp;lt;tag&amp;gt;"), "<tag>");
    }

    #[test]
    fn rich_text_preview_ignores_wps_body_metadata_prefix() {
        let html = "<html><body>1 1 1 1 MicrosoftInternetExplorer4 0 2 DocumentNotSpecified 7.8 磅 Normal 0 <span>顶顶顶顶</span></body></html>";

        let preview = build_entry_preview("rich_text", html, Some(html));

        assert_eq!(preview, "顶顶顶顶");
    }

    #[test]
    fn rich_text_content_preserves_obsidian_callout_markdown() {
        let text = "> [!note]- Important\n> Keep the markdown callout syntax";
        let html =
            "<blockquote><p>Important</p><p>Keep the markdown callout syntax</p></blockquote>";

        let content = derive_rich_text_content(text, Some(html));

        assert_eq!(content, text);
    }

    #[test]
    fn infer_rich_html_from_plain_text_promotes_wps_table_fragment() {
        let text =
            "table border=0 cellpadding=0 cellspacing=0><tr><td>学院意见</td><td>通过</td></tr>";

        let html = infer_rich_html_from_plain_text(
            text,
            "WPS Office",
            Some("C:\\Program Files\\Kingsoft\\wps.exe"),
        )
        .expect("wps html-ish text should promote to rich html");

        assert!(html.starts_with("<table"));
        assert!(html.contains("<td>学院意见</td>"));
        assert_eq!(
            collapse_preview_whitespace(&derive_rich_text_content(text, Some(&html))),
            "学院意见 通过"
        );
    }

    #[test]
    fn infer_rich_html_from_plain_text_keeps_html_source_from_editor_as_code() {
        let text = "<div class=\"note\">hello</div>";

        let html = infer_rich_html_from_plain_text(
            text,
            "Visual Studio Code",
            Some("C:\\Program Files\\Microsoft VS Code\\Code.exe"),
        );

        assert!(html.is_none());
    }

    #[test]
    fn rich_named_formats_round_trip_without_touching_html() {
        let html = "<table><tr><td>A</td><td>B</td></tr></table>";
        let formats = vec![
            crate::infrastructure::windows_api::win_clipboard::NamedClipboardFormat {
                name: "Rich Text Format".to_string(),
                data: b"{\\rtf1\\ansi A\\tab B}".to_vec(),
            },
            crate::infrastructure::windows_api::win_clipboard::NamedClipboardFormat {
                name: "Biff8".to_string(),
                data: vec![1, 2, 3, 4],
            },
        ];

        let tagged = attach_rich_named_formats(html, &formats);
        let (cleaned, restored) = split_rich_html_and_named_formats(&tagged);

        assert_eq!(cleaned, html);
        assert_eq!(restored, formats);
    }

    #[test]
    fn rich_named_formats_and_image_fallback_can_coexist() {
        let html = "<table><tr><td>Excel</td></tr></table>";
        let html = attach_rich_image_fallback(html, "data:image/png;base64,AAAA");
        let formats = vec![
            crate::infrastructure::windows_api::win_clipboard::NamedClipboardFormat {
                name: "Biff12".to_string(),
                data: vec![9, 8, 7],
            },
        ];

        let tagged = attach_rich_named_formats(&html, &formats);
        let (without_formats, restored_formats) = split_rich_html_and_named_formats(&tagged);
        let (cleaned, restored_image) = split_rich_html_and_image_fallback(&without_formats);

        assert_eq!(restored_formats, formats);
        assert_eq!(
            restored_image.as_deref(),
            Some("data:image/png;base64,AAAA")
        );
        assert_eq!(cleaned, "<table><tr><td>Excel</td></tr></table>");
    }

    #[test]
    fn extract_animated_image_data_url_from_html_prefers_data_gif() {
        let gif_data_url =
            "data:image/gif;base64,R0lGODlhAQABAPAAAP///wAAACH5BAAAAAAALAAAAAABAAEAAAICRAEAOw==";
        let html = format!(r#"<div><img src="{gif_data_url}" alt="gif" /></div>"#);

        let extracted = extract_animated_image_data_url_from_html(&html);

        assert_eq!(extracted.as_deref(), Some(gif_data_url));
    }

    #[test]
    fn extract_animated_image_data_url_from_html_ignores_static_png() {
        let html = r#"<div><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAUA" /></div>"#;

        let extracted = extract_animated_image_data_url_from_html(html);

        assert!(extracted.is_none());
    }

    #[test]
    fn extract_first_image_data_url_from_html_accepts_static_png_data_url() {
        let png_data_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAUA";
        let html = format!(r#"<div><img src="{png_data_url}" alt="png" /></div>"#);

        let extracted = extract_first_image_data_url_from_html(&html);

        assert_eq!(extracted.as_deref(), Some(png_data_url));
    }

    #[test]
    fn extract_first_image_data_url_from_html_reads_local_file_url() {
        let path = create_test_png_file("rich_local.png");
        let html = format!(
            r#"<div><img src="{}?v=1#preview" /></div>"#,
            file_url_for(&path)
        );

        let extracted = extract_first_image_data_url_from_html(&html);

        assert!(extracted
            .as_deref()
            .map(|value| value.starts_with("data:image/png;base64,"))
            .unwrap_or(false));

        cleanup_test_path(&path);
    }

    #[test]
    fn extract_animated_image_data_url_from_text_accepts_direct_gif_data_url() {
        let gif_data_url =
            "data:image/gif;base64,R0lGODlhAQABAPAAAP///wAAACH5BAAAAAAALAAAAAABAAEAAAICRAEAOw==";

        let extracted = extract_animated_image_data_url_from_text(gif_data_url);

        assert_eq!(extracted.as_deref(), Some(gif_data_url));
    }

    #[test]
    fn parse_cf_html_repairs_missing_opening_bracket() {
        let raw = b"Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n<!--StartFragment-->table border=0 cellpadding=0 cellspacing=0><tr><td>A</td></tr><!--EndFragment-->";
        let parsed = parse_cf_html(raw).expect("cf_html should parse");

        assert!(parsed.starts_with("<table"));
        assert!(parsed.contains("<td>A</td>"));
    }

    #[test]
    fn parse_cf_html_handles_fragment_offsets_without_line_break_separator() {
        let raw = b"Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000189\r\nStartFragment:0000000141EndFragment:0000000173\r\n<!--StartFragment--><p>Hello</p><!--EndFragment-->";
        let parsed = parse_cf_html(raw)
            .expect("cf_html should parse from markers when offsets are malformed");

        assert!(!parsed.contains("StartHTML:"));
        assert!(!parsed.contains("StartFragment:"));
        assert!(parsed.contains("<p>Hello</p>"));
    }

    #[test]
    fn parse_cf_html_does_not_return_raw_header_when_only_fragment_like_payload_survives() {
        let raw = b"Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000829\r\nStartFragment:0000000141EndFragment:0000000793\r\ntable border=0 cellpadding=0 cellspacing=0><tr><td>A</td></tr>";
        let parsed = parse_cf_html(raw).expect("cf_html should recover fragment-like payload");

        assert!(parsed.starts_with("<table"), "parsed={parsed:?}");
        assert!(parsed.contains("<td>A</td>"));
        assert!(!parsed.contains("Version:0.9"));
        assert!(!parsed.contains("StartHTML:"));
    }

    #[test]
    fn normalize_clipboard_plain_text_strips_cf_html_header_prefix() {
        let text = "Version:0.9 StartHTML:0000000105 EndHTML:0000000829 StartFragment:0000000141 EndFragment:0000000793 ddd";

        let normalized = normalize_clipboard_plain_text(text);

        assert_eq!(normalized, "ddd");
    }

    #[test]
    fn text_preview_drops_cf_html_header_noise_for_plain_text_items() {
        let text = "Version:0.9 StartHTML:0000000105 EndHTML:0000000829 StartFragment:0000000141 EndFragment:0000000793 ddd";

        let preview = build_entry_preview("text", text, None);

        assert_eq!(preview, "ddd");
    }

    #[test]
    fn cleanup_rules_parse_and_apply_replacements() {
        let rules = parse_cleanup_rules(
            r"(?i)token\s*:\s*\S+ => token: [REDACTED]
\b1[3-9]\d{9}\b => [PHONE]",
        );

        let cleaned = apply_cleanup_rules("token: abc123 13812345678", &rules);

        assert_eq!(cleaned, "token: [REDACTED] [PHONE]");
    }

    #[test]
    fn app_cleanup_policy_parse_filters_disabled_or_unbound_items() {
        let policies = parse_app_cleanup_policies(
            r#"[
                {"id":"1","enabled":true,"appName":"WeChat","contentTypes":["text"]},
                {"id":"2","enabled":false,"appName":"Slack","contentTypes":["text"]},
                {"id":"3","enabled":true,"contentTypes":["text"]}
            ]"#,
        );

        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].id, "1");
    }

    #[test]
    fn app_cleanup_policy_match_prefers_path_and_respects_content_type() {
        let policy = AppCleanupPolicy {
            id: "1".to_string(),
            enabled: true,
            app_name: "WeChat".to_string(),
            app_path: "C:\\Program Files\\Tencent\\WeChat.exe".to_string(),
            action: "ignore".to_string(),
            content_types: vec!["text".to_string(), "url".to_string()],
            cleanup_rules: String::new(),
        };

        assert!(app_cleanup_policy_matches(
            &policy,
            "Different Name",
            Some("C:\\Program Files\\Tencent\\WeChat.exe"),
            "text",
        ));
        assert!(!app_cleanup_policy_matches(
            &policy,
            "WeChat",
            Some("C:\\Program Files\\Tencent\\WeChat.exe"),
            "image",
        ));
    }

    #[test]
    fn app_cleanup_policy_match_accepts_executable_name_variant() {
        let policy = AppCleanupPolicy {
            id: "1".to_string(),
            enabled: true,
            app_name: "Codex".to_string(),
            app_path: String::new(),
            action: "clean".to_string(),
            content_types: vec!["text".to_string()],
            cleanup_rules: String::new(),
        };

        assert!(app_cleanup_policy_matches(
            &policy,
            "Codex.exe",
            Some(
                "C:\\Program Files\\WindowsApps\\OpenAI.Codex_26.305.950.0_x64__2p2nqsd0c76g0\\app\\Codex.exe",
            ),
            "text",
        ));
    }

    #[test]
    fn app_cleanup_policy_match_accepts_windows_app_id_variant() {
        let policy = AppCleanupPolicy {
            id: "1".to_string(),
            enabled: true,
            app_name: "Codex".to_string(),
            app_path: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
            action: "clean".to_string(),
            content_types: vec!["text".to_string()],
            cleanup_rules: String::new(),
        };

        assert!(app_cleanup_policy_matches(
            &policy,
            "Codex.exe",
            Some(
                "C:\\Program Files\\WindowsApps\\OpenAI.Codex_26.305.950.0_x64__2p2nqsd0c76g0\\app\\Codex.exe",
            ),
            "text",
        ));
    }
}

pub fn detect_content_type(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("www.") || trimmed.contains("://") && trimmed.split("://").next().map_or(false, |s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')) {
        return "url".to_string();
    }

    let mut score = 0;
    let keywords = [
        "import ",
        "const ",
        "let ",
        "var ",
        "function ",
        "class ",
        "pub fn ",
        "impl ",
        "#include",
        "package ",
        "interface ",
        "namespace ",
        "void ",
        "return ",
        "if (",
        "for (",
        "while (",
        "=>",
    ];

    for k in keywords {
        if text.contains(k) {
            score += 1;
        }
    }

    if text.contains(";") {
        score += 1;
    }
    if text.contains("{") && text.contains("}") {
        score += 1;
    }
    if text.contains("</") && text.contains(">") {
        score += 2;
    }

    if score >= 2 {
        return "code".to_string();
    }

    if trimmed.starts_with("{")
        && trimmed.ends_with("}")
        && text.contains(":")
        && text.contains("\"")
    {
        return "code".to_string();
    }

    "text".to_string()
}

pub fn contains_sensitive_info(text: &str, kinds: &[String], custom_rules: &[String]) -> bool {
    static PHONE_RE: OnceLock<Regex> = OnceLock::new();
    static IDCARD_RE: OnceLock<Regex> = OnceLock::new();
    static EMAIL_RE: OnceLock<Regex> = OnceLock::new();
    static SECRET_RE: OnceLock<Regex> = OnceLock::new();

    static URL_RE: OnceLock<Regex> = OnceLock::new();

    if text.len() > 5000 || text.starts_with("data:") {
        return false;
    }

    let has_kind = |k: &str| kinds.iter().any(|t| t == k);

    if has_kind("url") {
        let re = URL_RE.get_or_init(|| Regex::new(r"(?i)(?:[a-zA-Z][a-zA-Z0-9+\-.]*://|www\.)\S+").unwrap());
        if re.is_match(text) { return true; }
    }
    if has_kind("phone") {
        let re = PHONE_RE.get_or_init(|| {
            Regex::new(r"(?:\+?86)?[-\s\(]*1[3-9]\d{1}[-\s\)]*\d{4}[-\s]*\d{4}").unwrap()
        });
        if re.is_match(text) {
            return true;
        }
    }
    if has_kind("idcard") {
        let re = IDCARD_RE.get_or_init(|| {
            Regex::new(
                r"\b[1-9]\d{5}[1-9]\d{3}((0\d)|(1[0-2]))(([0|1|2]\d)|3[0-1])\d{3}([0-9Xx])\b",
            )
            .unwrap()
        });
        if re.is_match(text) {
            return true;
        }
    }
    if has_kind("email") {
        let re = EMAIL_RE
            .get_or_init(|| Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap());
        if re.is_match(text) {
            return true;
        }
    }
    if has_kind("secret") {
        let re = SECRET_RE.get_or_init(|| Regex::new(r"(?ix)((?:sk|pk|ghp|gho|github_pat|AIza|AKIA|ya29)[-_][\w\-]{20,}|(?:password|secret|api[_-]?key|access[_-]?key|token|bearer)[\s:=]+[\w\-]{16,})").unwrap());
        if re.is_match(text) {
            return true;
        }
    }
    if has_kind("password") {
        if text.len() >= 8 && text.len() <= 64 && !text.contains(' ') && !text.contains('\n') {
            let has_upper = text.chars().any(|c| c.is_uppercase());
            let has_lower = text.chars().any(|c| c.is_lowercase());
            let has_digit = text.chars().any(|c| c.is_numeric());
            let has_special = text.chars().any(|c| !c.is_alphanumeric());
            if has_upper && has_lower && has_digit && has_special {
                return true;
            }
        }
    }

    for rule in custom_rules {
        if let Ok(re) = Regex::new(rule) {
            if re.is_match(text) {
                return true;
            }
        }
    }
    false
}

pub fn parse_cleanup_rules(raw_rules: &str) -> Vec<(Regex, String)> {
    raw_rules
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (pattern, replacement) = line.split_once("=>")?;
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return None;
            }

            let replacement = replacement
                .trim()
                .replace(r"\n", "\n")
                .replace(r"\r", "\r")
                .replace(r"\t", "\t");

            Regex::new(pattern).ok().map(|regex| (regex, replacement))
        })
        .collect()
}

pub fn apply_cleanup_rules(text: &str, rules: &[(Regex, String)]) -> String {
    rules
        .iter()
        .fold(text.to_string(), |acc, (regex, replacement)| {
            regex.replace_all(&acc, replacement.as_str()).into_owned()
        })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppCleanupPolicy {
    #[cfg_attr(not(test), allow(dead_code))]
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_policy_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub app_path: String,
    #[serde(default = "default_policy_action")]
    pub action: String,
    #[serde(default = "default_policy_content_types")]
    pub content_types: Vec<String>,
    #[serde(default)]
    pub cleanup_rules: String,
}

fn default_policy_enabled() -> bool {
    true
}

fn default_policy_action() -> String {
    "clean".to_string()
}

fn default_policy_content_types() -> Vec<String> {
    vec![
        "text".to_string(),
        "code".to_string(),
        "url".to_string(),
        "rich_text".to_string(),
        "image".to_string(),
        "file".to_string(),
        "video".to_string(),
    ]
}

fn normalize_executable_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let segment = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed).trim();
    if segment.is_empty() {
        return None;
    }

    let lower = segment.to_ascii_lowercase();
    let normalized = lower.strip_suffix(".exe").unwrap_or(&lower).trim();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.to_string())
    }
}

fn executable_name_matches(left: &str, right: &str) -> bool {
    match (
        normalize_executable_name(left),
        normalize_executable_name(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn app_id_matches_process_path(app_id: &str, process_path: &str) -> bool {
    let trimmed_app_id = app_id.trim();
    let trimmed_process_path = process_path.trim();
    if trimmed_app_id.is_empty() || trimmed_process_path.is_empty() || !trimmed_app_id.contains('!')
    {
        return false;
    }

    let package_family = trimmed_app_id.split('!').next().unwrap_or("").trim();
    let Some((package_name, publisher_id)) = package_family.rsplit_once('_') else {
        return false;
    };

    let normalized_path = trimmed_process_path.replace('/', "\\").to_ascii_lowercase();
    let package_name = package_name.trim().to_ascii_lowercase();
    let publisher_id = publisher_id.trim().to_ascii_lowercase();

    !package_name.is_empty()
        && !publisher_id.is_empty()
        && normalized_path.contains(&package_name)
        && normalized_path.contains(&publisher_id)
}

pub fn parse_app_cleanup_policies(raw_policies: &str) -> Vec<AppCleanupPolicy> {
    serde_json::from_str::<Vec<AppCleanupPolicy>>(raw_policies)
        .unwrap_or_default()
        .into_iter()
        .filter(|policy| {
            policy.enabled
                && (!policy.app_path.trim().is_empty() || !policy.app_name.trim().is_empty())
        })
        .collect()
}

pub fn app_cleanup_policy_matches(
    policy: &AppCleanupPolicy,
    source_app: &str,
    source_app_path: Option<&str>,
    content_type: &str,
) -> bool {
    let allowed = if policy.action.eq_ignore_ascii_case("ignore") {
        // If we are ignoring an app, we should be aggressive in matching unless types are specifically filtered
        policy.content_types.is_empty()
            || policy
                .content_types
                .iter()
                .any(|kind| kind.eq_ignore_ascii_case(content_type))
    } else {
        !policy.content_types.is_empty()
            && policy
                .content_types
                .iter()
                .any(|kind| kind.eq_ignore_ascii_case(content_type))
    };
    if !allowed {
        return false;
    }

    let source_app = source_app.trim();
    let source_app_path = source_app_path.unwrap_or("").trim();
    let policy_path = policy.app_path.trim();
    if !policy_path.is_empty() && !source_app_path.is_empty() {
        if policy_path.len() >= 2 && policy_path.starts_with('/') && policy_path.ends_with('/') {
            let re_str = &policy_path[1..policy_path.len() - 1];
            if let Ok(re) = Regex::new(re_str) {
                if re.is_match(source_app_path) {
                    return true;
                }
            }
        }
        if policy_path.eq_ignore_ascii_case(source_app_path) {
            return true;
        }
        if executable_name_matches(policy_path, source_app_path)
            || app_id_matches_process_path(policy_path, source_app_path)
        {
            return true;
        }
    }

    let policy_name = policy.app_name.trim();
    if !policy_name.is_empty() {
        if policy_name.len() >= 2 && policy_name.starts_with('/') && policy_name.ends_with('/') {
            let re_str = &policy_name[1..policy_name.len() - 1];
            if let Ok(re) = Regex::new(re_str) {
                if re.is_match(source_app) {
                    return true;
                }
            }
        }
        if policy_name.eq_ignore_ascii_case(source_app) {
            return true;
        }
        if executable_name_matches(policy_name, source_app)
            || executable_name_matches(policy_name, source_app_path)
        {
            return true;
        }
    }

    if !policy_path.is_empty()
        && !source_app.is_empty()
        && executable_name_matches(policy_path, source_app)
    {
        return true;
    }
    false
}

pub fn embed_local_images(html: &str) -> String {
    let re = match Regex::new(r#"(<img\s+[^>]*src=["'])([^"']+)(["'][^>]*>)"#) {
        Ok(r) => r,
        Err(_) => return html.to_string(),
    };

    re.replace_all(html, |caps: &regex::Captures| {
        let prefix = &caps[1];
        let src = &caps[2];
        let suffix = &caps[3];

        let is_local = src.starts_with("file://")
            || (src.len() > 2
                && src.chars().nth(1) == Some(':')
                && (src.chars().nth(2) == Some('\\') || src.chars().nth(2) == Some('/')));

        if is_local {
            let path_str = if src.starts_with("file://") {
                let raw_path = src.trim_start_matches("file://");
                if raw_path.starts_with('/') && raw_path.chars().nth(2) == Some(':') {
                    &raw_path[1..]
                } else {
                    raw_path
                }
            } else {
                src
            };

            let decoded_path = decode(path_str)
                .map(|p| p.into_owned())
                .unwrap_or(path_str.to_string());
            let clean_path = decoded_path
                .split('?')
                .next()
                .unwrap_or(&decoded_path)
                .split('#')
                .next()
                .unwrap_or(&decoded_path);

            let path = std::path::Path::new(clean_path);
            if path.exists() {
                if let Ok(data) = std::fs::read(path) {
                    let ext = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("png")
                        .to_lowercase();
                    let mime = match ext.as_str() {
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        "bmp" => "image/bmp",
                        "svg" => "image/svg+xml",
                        _ => "image/png",
                    };
                    let b64 = general_purpose::STANDARD.encode(&data);
                    return format!(
                        "{}{}{}",
                        prefix,
                        format!("data:{};base64,{}", mime, b64),
                        suffix
                    );
                }
            }
        }

        if let Some(remote_url) = normalize_remote_img_url(src) {
            if let Some((bytes, ext)) = fetch_remote_image(&remote_url) {
                let b64 = general_purpose::STANDARD.encode(&bytes);
                let mime = image_mime_by_ext(ext);
                let data_url = format!("data:{};base64,{}", mime, b64);
                return format!("{}{}{}", prefix, data_url, suffix);
            }
        }
        format!("{}{}{}", prefix, src, suffix)
    })
    .to_string()
}

pub fn process_local_images_in_html(html: &str, data_dir: &std::path::Path) -> String {
    let attachments_dir = data_dir.join("attachments");
    if !attachments_dir.exists() {
        let _ = std::fs::create_dir_all(&attachments_dir);
    }

    let re = match Regex::new(r#"(<img\s+[^>]*src=["'])([^"']+)(["'][^>]*>)"#) {
        Ok(r) => r,
        Err(_) => return html.to_string(),
    };

    re.replace_all(html, |caps: &regex::Captures| {
        let prefix = &caps[1];
        let src = &caps[2];
        let suffix = &caps[3];

        let is_local = src.starts_with("file://")
            || (src.len() > 2
                && src.chars().nth(1) == Some(':')
                && (src.chars().nth(2) == Some('\\') || src.chars().nth(2) == Some('/')));

        if is_local {
            let path_str = if src.starts_with("file://") {
                let raw_path = src.trim_start_matches("file://");
                if raw_path.starts_with('/') && raw_path.chars().nth(2) == Some(':') {
                    &raw_path[1..]
                } else {
                    raw_path
                }
            } else {
                src
            };

            let decoded_path = decode(path_str)
                .map(|p| p.into_owned())
                .unwrap_or(path_str.to_string());
            let clean_path = decoded_path
                .split('?')
                .next()
                .unwrap_or(&decoded_path)
                .split('#')
                .next()
                .unwrap_or(&decoded_path);
            let path = std::path::Path::new(clean_path);

            if path.starts_with(&attachments_dir) {
                return format!("{}{}{}", prefix, src, suffix);
            }

            if path.exists() {
                if let Ok(data) = std::fs::read(path) {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    use std::hash::{Hash, Hasher};
                    data.hash(&mut hasher);
                    let hash = hasher.finish();

                    let ext = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("png")
                        .to_lowercase();
                    let new_filename = format!("img_{:x}.{}", hash, ext);
                    let new_path = attachments_dir.join(&new_filename);

                    if !new_path.exists() {
                        let _ = std::fs::write(&new_path, &data);
                    }

                    let new_src = new_path.to_string_lossy().replace('\\', "/");
                    let final_src = if new_src.starts_with('/') {
                        format!("file://{}", new_src)
                    } else {
                        format!("file:///{}", new_src)
                    };
                    return format!("{}{}{}", prefix, final_src, suffix);
                }
            }
        }

        if let Some(remote_url) = normalize_remote_img_url(src) {
            if let Some((bytes, ext)) = fetch_remote_image(&remote_url) {
                if let Some(file_src) =
                    save_image_bytes_to_attachments(&bytes, ext, &attachments_dir)
                {
                    return format!("{}{}{}", prefix, file_src, suffix);
                }
            }
        }
        format!("{}{}{}", prefix, src, suffix)
    })
    .to_string()
}

pub fn parse_cf_html(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    enum HtmlEncoding {
        Utf8,
        Utf16Le,
    }

    let detect_encoding = |data: &[u8]| -> HtmlEncoding {
        if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
            return HtmlEncoding::Utf16Le;
        }
        // Heuristic for UTF-16LE
        if data.len() >= 4 && data[1] == 0 && data[3] == 0 {
            return HtmlEncoding::Utf16Le;
        }
        HtmlEncoding::Utf8
    };

    let encoding = detect_encoding(raw);
    let raw_str = match encoding {
        HtmlEncoding::Utf8 => String::from_utf8_lossy(raw).to_string(),
        HtmlEncoding::Utf16Le => {
            let u16_buf: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&u16_buf)
        }
    };

    let parse_offset = |key: &str| -> Option<usize> {
        let idx = raw_str.find(key)?;
        let val_start = idx + key.len();
        let val_str: String = raw_str[val_start..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || c.is_whitespace())
            .collect();
        val_str.trim().parse::<usize>().ok()
    };

    let start_html = parse_offset("StartHTML:");
    let end_html = parse_offset("EndHTML:");
    let start_frag = parse_offset("StartFragment:");
    let end_frag = parse_offset("EndFragment:");

    // Prefer the full HTML range to preserve document-wide styles (CSS)
    let (s, e, is_full_doc) = if let (Some(s_h), Some(e_h)) = (start_html, end_html) {
        (s_h, e_h, true)
    } else if let (Some(s_f), Some(e_f)) = (start_frag, end_frag) {
        (s_f, e_f, false)
    } else {
        (0, 0, false)
    };

    if s < e {
        let content = match encoding {
            HtmlEncoding::Utf8 => {
                if e <= raw_str.len() {
                    Some(raw_str[s..e].to_string())
                } else {
                    None
                }
            }
            HtmlEncoding::Utf16Le => {
                if e <= raw.len() {
                    let u16_buf: Vec<u16> = raw[s..e]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    Some(String::from_utf16_lossy(&u16_buf))
                } else {
                    None
                }
            }
        };

        if let Some(c) = content {
            if is_full_doc {
                return Some(c);
            } else {
                return Some(repair_html_fragment(&c));
            }
        }
    }

    // Fallback search for fragments if offsets failed or produced invalid results
    let start_marker = "<!--StartFragment-->";
    let end_marker = "<!--EndFragment-->";
    if let Some(s_idx) = raw_str.find(start_marker) {
        let after_s = s_idx + start_marker.len();
        if let Some(e_idx) = raw_str[after_s..].find(end_marker) {
            return Some(repair_html_fragment(&raw_str[after_s..after_s + e_idx]));
        }
    }

    // Last resort heuristics
    if raw_str.contains("Version:") {
        let mut in_header = true;
        let mut cleaned_lines = Vec::new();
        for line in raw_str.lines() {
            let trimmed = line.trim();
            if in_header {
                let lower = trimmed.to_ascii_lowercase();
                let is_header_key = lower.starts_with("version:")
                    || lower.starts_with("starthtml:")
                    || lower.starts_with("endhtml:")
                    || lower.starts_with("startfragment:")
                    || lower.starts_with("endfragment:")
                    || lower.starts_with("sourceurl:");

                if is_header_key || trimmed.is_empty() {
                    continue;
                }
                in_header = false;
            }
            cleaned_lines.push(line);
        }

        let cleaned = cleaned_lines.join("\n").trim().to_string();
        if looks_like_html_fragment_shallow(&cleaned) {
            return Some(repair_html_fragment(&cleaned));
        }

        if let Some(first_bracket) = raw_str.find('<') {
            let potential = &raw_str[first_bracket..];
            if looks_like_html_fragment_shallow(potential) {
                return Some(repair_html_fragment(potential));
            }
        }
    }

    if looks_like_html_fragment_shallow(&raw_str) {
        return Some(repair_html_fragment(&raw_str));
    }

    None
}

#[cfg(test)]
mod tests_content_type {
    use super::*;

    mod detect_content_type_tests {
        use super::*;

        #[test]
        fn http_url() {
            assert_eq!(detect_content_type("http://example.com"), "url");
        }

        #[test]
        fn https_url() {
            assert_eq!(detect_content_type("https://example.com/path?q=1"), "url");
        }

        #[test]
        fn ftp_url() {
            assert_eq!(detect_content_type("ftp://files.example.com/doc.pdf"), "url");
        }

        #[test]
        fn custom_protocol_url() {
            assert_eq!(detect_content_type("myapp+custom://open/page"), "url");
        }

        #[test]
        fn www_url() {
            assert_eq!(detect_content_type("www.example.com"), "url");
        }

        #[test]
        fn url_with_whitespace() {
            assert_eq!(detect_content_type("  https://example.com  "), "url");
        }

        #[test]
        fn plain_text_not_url() {
            assert_eq!(detect_content_type("hello world"), "text");
        }

        #[test]
        fn colon_slash_slash_in_plain_text_no_valid_scheme() {
            // "://foo" alone — the part before :// is empty
            assert_eq!(detect_content_type("://foo"), "text");
        }

        #[test]
        fn code_snippet() {
            assert_eq!(detect_content_type("const x = 1; function foo() {}"), "code");
        }
    }

    mod contains_sensitive_info_tests {
        use super::*;

        fn kinds(list: &[&str]) -> Vec<String> {
            list.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn detects_url() {
            assert!(contains_sensitive_info(
                "visit https://secret.internal/admin",
                &kinds(&["url"]),
                &[],
            ));
        }

        #[test]
        fn detects_ftp_url() {
            assert!(contains_sensitive_info(
                "ftp://files.company.com/secret.zip",
                &kinds(&["url"]),
                &[],
            ));
        }

        #[test]
        fn detects_www_url() {
            assert!(contains_sensitive_info(
                "visit www.example.com/admin",
                &kinds(&["url"]),
                &[],
            ));
        }

        #[test]
        fn no_url_kind_skips_url_check() {
            assert!(!contains_sensitive_info(
                "https://example.com",
                &kinds(&["phone"]),
                &[],
            ));
        }

        #[test]
        fn detects_phone() {
            assert!(contains_sensitive_info(
                "call me 13812345678",
                &kinds(&["phone"]),
                &[],
            ));
        }

        #[test]
        fn detects_email() {
            assert!(contains_sensitive_info(
                "send to user@example.com",
                &kinds(&["email"]),
                &[],
            ));
        }

        #[test]
        fn skips_data_uri() {
            assert!(!contains_sensitive_info(
                "data:image/png;base64,iVBOR...",
                &kinds(&["url", "phone", "email"]),
                &[],
            ));
        }

        #[test]
        fn skips_oversized_text() {
            let big = "a".repeat(5001);
            assert!(!contains_sensitive_info(
                &big,
                &kinds(&["phone"]),
                &[],
            ));
        }

        #[test]
        fn custom_regex_rule() {
            assert!(contains_sensitive_info(
                "order-12345",
                &kinds(&[]),
                &["order-\\d+".to_string()],
            ));
        }
    }
}
