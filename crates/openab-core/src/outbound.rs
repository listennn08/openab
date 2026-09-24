//! Outbound file attachments: resolve agent-referenced paths, enforce the
//! workspace scope boundary, and extract attach candidates from reply text.
//!
//! Two ways an agent can attach a file to its reply:
//!   - `[[attach:path]]` output directive (leading header block, like reply_to)
//!   - `![alt](path)` markdown image whose target is a local path
//!
//! Both resolve relative to the session working directory. `~` expands to the
//! bot home. Canonicalized paths must stay inside the session workdir or the
//! bot home — anything else is rejected so the agent (or injected text in its
//! output) cannot exfiltrate arbitrary files like /etc/passwd or ~/.ssh keys
//! outside the workspace boundary.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Maximum attachment size delivered to a platform (Discord parity: 25 MiB).
/// Slack accepts far larger files, but one uniform cap keeps behavior
/// predictable across adapters.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// Maximum attachments delivered per turn.
pub const MAX_ATTACHMENTS_PER_TURN: usize = 10;

static MD_IMAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // ![alt](path) or ![alt](path "title") — path itself has no spaces/parens.
    Regex::new(r#"!\[[^\]]*\]\(([^)\s"]+)(?:\s+"[^"]*")?\)"#).unwrap()
});

/// A resolved, in-scope file ready for upload.
#[derive(Debug)]
pub struct PendingUpload {
    /// Canonical absolute path on disk.
    pub path: PathBuf,
    /// Display filename (sanitized basename).
    pub filename: String,
}

/// Result of scanning a reply body for attach candidates.
pub struct ExtractedAttachments {
    /// Body with successfully-resolved markdown images removed.
    pub text: String,
    /// Files to upload (deduplicated by canonical path, order preserved).
    pub uploads: Vec<PendingUpload>,
    /// User-facing warnings for attach attempts that failed (explicit
    /// directives always warn; markdown images warn only when the path
    /// resolved to a real file that was rejected).
    pub notes: Vec<String>,
}

/// Why a path was rejected.
#[derive(Debug, PartialEq)]
pub enum Reject {
    /// File does not exist (or canonicalize failed).
    NotFound,
    /// Resolved path escapes the allowed roots (workdir / bot home).
    OutsideWorkspace,
    /// Resolved path is not a regular file.
    NotAFile,
    /// File exceeds MAX_ATTACHMENT_BYTES.
    TooLarge(u64),
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "file not found"),
            Self::OutsideWorkspace => write!(f, "outside allowed workspace"),
            Self::NotAFile => write!(f, "not a regular file"),
            Self::TooLarge(size) => write!(
                f,
                "exceeds {} MB limit ({} MB)",
                MAX_ATTACHMENT_BYTES / (1024 * 1024),
                size / (1024 * 1024)
            ),
        }
    }
}

/// Resolve an agent-supplied path against the workspace boundary.
///
/// `workdir` is the session's effective working directory; `bot_home` backs
/// `~` expansion and is a second allowed root.
pub fn resolve_attachment_path(
    raw: &str,
    workdir: &Path,
    bot_home: &Path,
) -> Result<PathBuf, Reject> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Reject::NotFound);
    }

    let candidate = if let Some(rest) = raw.strip_prefix("~/") {
        bot_home.join(rest)
    } else if raw == "~" {
        bot_home.to_path_buf()
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workdir.join(raw)
    };

    // Canonicalize resolves `..` and symlinks — a symlink inside the workspace
    // pointing outside is correctly rejected by the scope check below.
    let canonical = candidate.canonicalize().map_err(|_| Reject::NotFound)?;

    let in_scope = [workdir, bot_home].iter().any(|root| {
        root.canonicalize()
            .map(|root| canonical.starts_with(root))
            .unwrap_or(false)
    });
    if !in_scope {
        return Err(Reject::OutsideWorkspace);
    }

    let meta = std::fs::metadata(&canonical).map_err(|_| Reject::NotFound)?;
    if !meta.is_file() {
        return Err(Reject::NotAFile);
    }
    if meta.len() > MAX_ATTACHMENT_BYTES {
        return Err(Reject::TooLarge(meta.len()));
    }

    Ok(canonical)
}

/// Basename for display, stripped of control chars, bounded length.
pub fn display_filename(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let cleaned: String = name.chars().filter(|c| !c.is_control()).take(200).collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// Best-effort MIME type from the filename extension. Platforms sniff content
/// anyway; this is only a hint for upload APIs that accept one.
pub fn guess_mime(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("json") => "application/json",
        Some("txt" | "log" | "md") => "text/plain",
        Some("csv") => "text/csv",
        _ => "application/octet-stream",
    }
}

/// Whether a markdown image target looks like a local path rather than a URL.
/// Remote (`http(s)://`, `data:`, protocol-relative) and anchor targets are
/// left for the platform to render (or ignore) as before.
fn is_local_path_target(target: &str) -> bool {
    !(target.contains("://")
        || target.starts_with("//")
        || target.starts_with("data:")
        || target.starts_with('#'))
}

/// Scan `text` for `[[attach:…]]`-provided paths (explicit) and `![…](path)`
/// markdown images (implicit), resolve each against the workspace, and return
/// the cleaned body plus the uploads to deliver.
///
/// Markdown images are stripped from the body only when their target resolves
/// to a real in-scope file that will be uploaded — remote URLs, anchors,
/// missing files, and rejected paths stay visible verbatim.
pub fn extract_attachments(
    text: &str,
    directive_paths: &[String],
    workdir: &Path,
    bot_home: &Path,
) -> ExtractedAttachments {
    let mut uploads: Vec<PendingUpload> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    let push_resolved =
        |resolved: PathBuf, uploads: &mut Vec<PendingUpload>, seen: &mut Vec<PathBuf>| {
            if seen.contains(&resolved) || uploads.len() >= MAX_ATTACHMENTS_PER_TURN {
                return;
            }
            seen.push(resolved.clone());
            uploads.push(PendingUpload {
                filename: display_filename(&resolved),
                path: resolved,
            });
        };

    // 1. Explicit [[attach:path]] directives — every failure is surfaced.
    for raw in directive_paths {
        match resolve_attachment_path(raw, workdir, bot_home) {
            Ok(p) => push_resolved(p, &mut uploads, &mut seen),
            Err(e) => notes.push(format!("Couldn't attach `{raw}` — {e}")),
        }
    }

    // 2. Markdown images with local-path targets.
    let mut cleaned = text.to_string();
    // Collect replacements first so span offsets stay valid.
    let mut replacements: Vec<(String, String)> = Vec::new(); // (full match, replacement)
    for caps in MD_IMAGE_RE.captures_iter(text) {
        let full = caps.get(0).unwrap().as_str().to_string();
        let target = caps[1].to_string();
        if !is_local_path_target(&target) {
            continue;
        }
        match resolve_attachment_path(&target, workdir, bot_home) {
            Ok(p) => {
                push_resolved(p, &mut uploads, &mut seen);
                // Strip the image markup; the upload itself carries the file.
                replacements.push((full, String::new()));
            }
            Err(Reject::NotFound) => {
                // Probably not meant as an upload (or a hallucinated path) —
                // leave the text untouched, no user-facing note.
            }
            Err(e) => {
                // Real file but rejected — leave the markup and explain.
                notes.push(format!("Couldn't attach `{target}` — {e}"));
            }
        }
    }
    for (full, replacement) in replacements {
        cleaned = cleaned.replacen(&full, &replacement, 1);
    }

    ExtractedAttachments {
        text: cleaned,
        uploads,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("ws");
        let home = dir.path().join("home");
        fs::create_dir_all(&workdir).unwrap();
        fs::create_dir_all(&home).unwrap();
        (dir, workdir, home)
    }

    #[test]
    fn resolves_relative_path_under_workdir() {
        let (_d, workdir, home) = setup();
        fs::write(workdir.join("chart.png"), b"png").unwrap();
        let p = resolve_attachment_path("chart.png", &workdir, &home).unwrap();
        assert!(p.ends_with("chart.png"));
    }

    #[test]
    fn resolves_tilde_under_home() {
        let (_d, workdir, home) = setup();
        fs::write(home.join("out.txt"), b"hi").unwrap();
        let p = resolve_attachment_path("~/out.txt", &workdir, &home).unwrap();
        assert!(p.ends_with("out.txt"));
    }

    #[test]
    fn rejects_parent_escape() {
        let (d, workdir, home) = setup();
        fs::write(d.path().join("outside.txt"), b"x").unwrap();
        // workdir/../outside.txt = dir/outside.txt — outside BOTH allowed roots.
        let err = resolve_attachment_path("../outside.txt", &workdir, &home).unwrap_err();
        assert_eq!(err, Reject::OutsideWorkspace);
    }

    #[test]
    fn relative_escape_into_home_is_in_scope() {
        let (_d, workdir, home) = setup();
        fs::write(home.join("ok.txt"), b"x").unwrap();
        // workdir/../home/ok.txt lands inside bot_home — an allowed root.
        let p = resolve_attachment_path("../home/ok.txt", &workdir, &home).unwrap();
        assert!(p.ends_with("ok.txt"));
    }

    #[test]
    fn rejects_outside_workspace_absolute() {
        let (_d, workdir, home) = setup();
        let err = resolve_attachment_path("/etc/hostname", &workdir, &home).unwrap_err();
        assert_eq!(err, Reject::OutsideWorkspace);
    }

    #[test]
    fn rejects_symlink_escape() {
        let (_d, workdir, home) = setup();
        let outside = _d.path().join("outside.txt");
        fs::write(&outside, b"x").unwrap();
        let link = workdir.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(not(unix))]
        return;
        let err = resolve_attachment_path("link.txt", &workdir, &home).unwrap_err();
        assert_eq!(err, Reject::OutsideWorkspace);
    }

    #[test]
    fn rejects_missing_file() {
        let (_d, workdir, home) = setup();
        let err = resolve_attachment_path("nope.png", &workdir, &home).unwrap_err();
        assert_eq!(err, Reject::NotFound);
    }

    #[test]
    fn remote_image_targets_untouched() {
        let (_d, workdir, home) = setup();
        let text = "see ![img](https://example.com/a.png) and ![x](data:image/png;base64,AAA)";
        let out = extract_attachments(text, &[], &workdir, &home);
        assert_eq!(out.text, text);
        assert!(out.uploads.is_empty());
        assert!(out.notes.is_empty());
    }

    #[test]
    fn local_markdown_image_extracted_and_stripped() {
        let (_d, workdir, home) = setup();
        fs::write(workdir.join("chart.png"), b"png").unwrap();
        let text = "here's the chart:\n![chart](chart.png)\nDone.";
        let out = extract_attachments(text, &[], &workdir, &home);
        assert_eq!(out.uploads.len(), 1);
        assert_eq!(out.uploads[0].filename, "chart.png");
        assert!(!out.text.contains("![chart]"));
        assert!(out.text.contains("here's the chart:"));
    }

    #[test]
    fn markdown_title_suffix_consumed() {
        let (_d, workdir, home) = setup();
        fs::write(workdir.join("a.png"), b"png").unwrap();
        let text = "![a](a.png \"the chart\")";
        let out = extract_attachments(text, &[], &workdir, &home);
        assert_eq!(out.uploads.len(), 1);
        assert!(!out.text.contains('!'));
    }

    #[test]
    fn out_of_scope_markdown_left_with_note() {
        let (_d, workdir, home) = setup();
        let text = "look ![passwd](/etc/hostname)";
        let out = extract_attachments(text, &[], &workdir, &home);
        assert!(out.uploads.is_empty());
        assert_eq!(out.text, text);
        assert_eq!(out.notes.len(), 1);
    }

    #[test]
    fn directive_paths_uploaded_and_failures_noted() {
        let (_d, workdir, home) = setup();
        fs::write(workdir.join("ok.png"), b"png").unwrap();
        let dirs = vec![
            "ok.png".to_string(),
            "/etc/hostname".to_string(),
            "missing.png".to_string(),
        ];
        let out = extract_attachments("body", &dirs, &workdir, &home);
        assert_eq!(out.uploads.len(), 1);
        assert_eq!(out.notes.len(), 2);
    }

    #[test]
    fn dedupes_same_file_across_sources() {
        let (_d, workdir, home) = setup();
        fs::write(workdir.join("a.png"), b"png").unwrap();
        let text = "![a](a.png)";
        let dirs = vec!["a.png".to_string()];
        let out = extract_attachments(text, &dirs, &workdir, &home);
        assert_eq!(out.uploads.len(), 1);
    }

    #[test]
    fn caps_attachment_count() {
        let (_d, workdir, home) = setup();
        let dirs: Vec<String> = (0..15)
            .map(|i| {
                let n = format!("f{i}.txt");
                fs::write(workdir.join(&n), b"x").unwrap();
                n
            })
            .collect();
        let out = extract_attachments("", &dirs, &workdir, &home);
        assert_eq!(out.uploads.len(), MAX_ATTACHMENTS_PER_TURN);
    }
}
