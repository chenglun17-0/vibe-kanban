//! Scan a repository's `docs/exec-plan/` directory for tech-solution plan
//! documents and parse their header metadata (状态 / 最近更新).
//!
//! Plans with status `待运行` are considered runnable: they surface in the
//! kanban To Do column and can be started as tasks.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

/// Status value marking a plan as ready to be picked up for execution.
pub const RUNNABLE_STATUS: &str = "待运行";

const PLAN_ROOT: &str = "docs/exec-plan";

#[derive(Debug, Error)]
pub enum ExecPlanError {
    #[error("invalid plan path: {0}")]
    InvalidPath(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPlanDoc {
    /// Repo-relative path, e.g. "docs/exec-plan/agents/foo.md"
    pub path: String,
    /// First-level heading (`# ...`)
    pub title: String,
    /// Header `- 状态：<value>`
    pub status: String,
    /// Header `- 最近更新：<value>`
    pub updated: Option<String>,
}

/// Parse title / status / updated from plan markdown.
///
/// Header metadata lines (`- <key>：<value>`) are only recognized before the
/// first `## ` section, mirroring the tech-solution validator. Returns None
/// when the document has no status header (not a tech-solution plan).
pub fn parse_plan(relative_path: &str, content: &str) -> Option<ExecPlanDoc> {
    let mut title = None;
    let mut status = None;
    let mut updated = None;
    for line in content.lines() {
        if line.starts_with("## ") {
            break;
        }
        if title.is_none()
            && let Some(rest) = line.strip_prefix("# ")
        {
            title = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ")
            && let Some((key, value)) = rest.split_once([':', '：'])
        {
            match key.trim() {
                "状态" => status = Some(value.trim().to_string()),
                "最近更新" => updated = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    status.map(|status| ExecPlanDoc {
        path: relative_path.to_string(),
        title: title.unwrap_or_else(|| relative_path.to_string()),
        status,
        updated,
    })
}

/// Recursively collect plan docs under `dir`, `relative_prefix` is the
/// repo-relative prefix of `dir`. Directories named `archive` are skipped.
fn collect(dir: &Path, relative_prefix: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return, // missing or unreadable dir: no plans
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if entry.file_name() == "archive" {
                continue;
            }
            collect(&path, &relative_prefix.join(entry.file_name()), out);
        } else if file_type.is_file()
            && path.extension().is_some_and(|ext| ext == "md")
            && let Ok(relative) = path.strip_prefix(dir)
        {
            out.push(relative_prefix.join(relative));
        }
    }
}

/// Scan `<repo_root>/docs/exec-plan/` for plan docs whose status equals
/// `wanted_status`. Missing directories or unreadable files are skipped.
pub fn scan_plans(repo_root: &Path, wanted_status: &str) -> Vec<ExecPlanDoc> {
    let mut paths = Vec::new();
    collect(&repo_root.join(PLAN_ROOT), Path::new(PLAN_ROOT), &mut paths);
    let mut docs: Vec<ExecPlanDoc> = paths
        .iter()
        .filter_map(|relative| {
            let content = fs::read_to_string(repo_root.join(relative)).ok()?;
            parse_plan(&relative.to_string_lossy(), &content)
        })
        .filter(|doc| doc.status == wanted_status)
        .collect();
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    docs
}

/// Read a single plan doc by repo-relative path, rejecting path traversal
/// outside `<repo_root>/docs/exec-plan/`.
pub fn read_plan(repo_root: &Path, relative_path: &str) -> Result<String, ExecPlanError> {
    let relative = Path::new(relative_path);
    let invalid = || ExecPlanError::InvalidPath(relative_path.to_string());
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || !relative.starts_with(PLAN_ROOT)
        || relative.extension() != Some("md".as_ref())
    {
        return Err(invalid());
    }
    let canonical_root = repo_root.canonicalize()?;
    let canonical = repo_root.join(relative).canonicalize()?;
    if !canonical.starts_with(&canonical_root) {
        return Err(invalid());
    }
    Ok(fs::read_to_string(canonical)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "# 演示计划\n\n- 状态：待运行\n- 最近更新：2026-08-12\n- 关联变更：无\n\n## 概述\n\n问题：x。\n";

    #[test]
    fn parses_header_before_first_section() {
        let doc = parse_plan("docs/exec-plan/a/demo.md", PLAN).unwrap();
        assert_eq!(doc.title, "演示计划");
        assert_eq!(doc.status, "待运行");
        assert_eq!(doc.updated.as_deref(), Some("2026-08-12"));
    }

    #[test]
    fn status_after_section_is_ignored() {
        let content = "# t\n\n## 概述\n\n- 状态：待运行\n";
        assert!(parse_plan("docs/exec-plan/a/t.md", content).is_none());
    }

    #[test]
    fn accepts_ascii_colon_and_halfwidth_variants() {
        let doc = parse_plan("docs/exec-plan/a/t.md", "# t\n\n- 状态: 草案\n").unwrap();
        assert_eq!(doc.status, "草案");
    }

    #[test]
    fn read_plan_rejects_traversal_and_non_plan_paths() {
        let root = Path::new("/tmp/nonexistent-vibe-kanban-exec-plans-test");
        assert!(matches!(
            read_plan(root, "../etc/passwd"),
            Err(ExecPlanError::InvalidPath(_))
        ));
        assert!(matches!(
            read_plan(root, "docs/exec-plan/../../x.md"),
            Err(ExecPlanError::InvalidPath(_))
        ));
        assert!(matches!(
            read_plan(root, "src/main.md"),
            Err(ExecPlanError::InvalidPath(_))
        ));
        assert!(matches!(
            read_plan(root, "docs/exec-plan/a/x.txt"),
            Err(ExecPlanError::InvalidPath(_))
        ));
        assert!(matches!(
            read_plan(root, "/abs/docs/exec-plan/a/x.md"),
            Err(ExecPlanError::InvalidPath(_))
        ));
    }

    #[test]
    fn read_plan_reads_existing_plan() {
        let dir = std::env::temp_dir().join(format!("exec-plans-test-{}", std::process::id()));
        let plan_dir = dir.join("docs/exec-plan/a");
        fs::create_dir_all(&plan_dir).unwrap();
        fs::write(plan_dir.join("p.md"), PLAN).unwrap();
        let content = read_plan(&dir, "docs/exec-plan/a/p.md").unwrap();
        assert!(content.contains("演示计划"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
