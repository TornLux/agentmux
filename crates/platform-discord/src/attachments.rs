//! Discord attachment → claude prompt augmentation.
//!
//! All accepted attachments are saved under
//! `%TEMP%\agentmux-attachments\<msg_id>-<sanitised>` and referenced
//! in the prompt as `[image: <abs_path>]` or `[file: <abs_path>]`
//! tags. Claude is expected to use the `Read` tool on those paths
//! (it handles images natively).
//!
//! Tags are appended on a new line after the user's text (separator:
//! `\n`) so the prompt reads as `<user text>\n[image: <path>]`.
//! Multi-line submission relies on broker writing the trailing `\r`
//! Enter as a separate PTY write call (see broker http_input).
//!
//! Buckets:
//!   * **image** (content_type `image/*` or known image extension)
//!   * **file**  (content_type `text/*` / `application/json` or known
//!     code extension; ≤ MAX_DOWNLOAD_BYTES — same path-tag format
//!     as images, claude reads on demand)
//!   * **skipped** — anything else (or download failures). The caller
//!     can use the returned list to show a ⚠️ to the user.
//!
//! Network failures degrade gracefully: a single attachment failing
//! to download lands in `skipped` rather than aborting the message.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serenity::all::Attachment;

/// Cap on how big a text attachment we'll bother downloading. The
/// content isn't inlined into the prompt anymore (claude reads on
/// demand), but we still enforce a sane upper bound on the local
/// temp file we create for it.
const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024;

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "kt", "c", "h",
    "hpp", "cpp", "cs", "rb", "php", "sh", "ps1", "psm1", "yaml", "yml", "toml", "json",
    "csv", "ini", "cfg", "conf", "html", "css", "scss", "sql", "xml", "log", "diff", "patch",
];

pub struct Processed {
    /// Final prompt text to send to claude — original message text
    /// followed (each on its own line) by `[image: ...]` / `[file: ...]`
    /// path tags. Multi-line is fine; broker writes Enter separately.
    pub prompt: String,
    /// Filenames that we couldn't / wouldn't include. Empty in the
    /// happy path.
    pub skipped: Vec<String>,
}

pub async fn process(text: &str, attachments: &[Attachment], msg_id: u64) -> Processed {
    let mut prompt = text.trim_end().to_string();
    let mut skipped = Vec::new();

    if attachments.is_empty() {
        return Processed { prompt, skipped };
    }

    let temp_root = std::env::temp_dir().join("agentmux-attachments");
    if let Err(e) = tokio::fs::create_dir_all(&temp_root).await {
        tracing::warn!("create attachment dir {}: {e}", temp_root.display());
        // Skip everything if we can't even prep the dir.
        return Processed {
            prompt,
            skipped: attachments.iter().map(|a| a.filename.clone()).collect(),
        };
    }

    for att in attachments {
        let lower = att.filename.to_ascii_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        let ct = att.content_type.as_deref().unwrap_or("");

        let is_image = ct.starts_with("image/") || IMAGE_EXTS.contains(&ext);
        let is_text = ct.starts_with("text/")
            || ct == "application/json"
            || TEXT_EXTS.contains(&ext);

        let kind = if is_image {
            Some("image")
        } else if is_text && (att.size as u64) <= MAX_DOWNLOAD_BYTES {
            Some("file")
        } else {
            None
        };

        let Some(kind) = kind else {
            skipped.push(att.filename.clone());
            continue;
        };

        match download_to_file(&temp_root, msg_id, att).await {
            Ok(path) => {
                if !prompt.is_empty() {
                    prompt.push('\n');
                }
                prompt.push_str(&format!("[{kind}: {}]", path.display()));
            }
            Err(e) => {
                tracing::warn!("attachment {} ({kind}): {e:#}", att.filename);
                skipped.push(att.filename.clone());
            }
        }
    }

    Processed { prompt, skipped }
}

async fn download_to_file(
    root: &std::path::Path,
    msg_id: u64,
    att: &Attachment,
) -> Result<PathBuf> {
    let path = root.join(format!("{}-{}", msg_id, sanitize(&att.filename)));
    let bytes = reqwest::get(&att.url)
        .await
        .with_context(|| format!("GET {}", att.url))?
        .error_for_status()
        .context("non-2xx from Discord CDN")?
        .bytes()
        .await
        .context("read body")?;
    tokio::fs::write(&path, &bytes)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize("a b/c.png"), "a_b_c.png");
        assert_eq!(sanitize("héllo.txt"), "h_llo.txt");
        assert_eq!(sanitize(""), "file");
    }
}
