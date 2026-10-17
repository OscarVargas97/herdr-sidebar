//! The branch's GitHub pull request and the issues it closes, through the
//! `gh` CLI: drawer lines for Source Control and Markdown files the preview
//! pane opens like any other document.
//!
//! `gh` takes a second or more, so drawer lines come from a cache that a
//! background thread refreshes; the heartbeat redraw picks up new results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a fetched result is shown before the next background refresh.
const FRESH_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Pr(u64),
    Issue(u64),
}

struct Entry {
    lines: Vec<String>,
    fetched: Option<Instant>,
    loading: bool,
}

fn cache() -> &'static Mutex<HashMap<(PathBuf, String), Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), Entry>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Drawer lines for `branch` checked out at `root`. Never blocks on `gh`:
/// returns the cached lines (or a loading note) and refreshes in the background.
pub fn pr_lines(root: &Path, branch: &str) -> Vec<String> {
    let key = (root.to_path_buf(), branch.to_string());
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.entry(key.clone()).or_insert(Entry {
        lines: vec!["(loading pull request…)".into()],
        fetched: None,
        loading: false,
    });
    let stale = entry.fetched.is_none_or(|t| t.elapsed() >= FRESH_FOR);
    if stale && !entry.loading {
        entry.loading = true;
        std::thread::spawn(move || {
            let lines = fetch_lines(&key.0);
            let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = map.get_mut(&key) {
                entry.lines = lines;
                entry.fetched = Some(Instant::now());
                entry.loading = false;
            }
        });
    }
    entry.lines.clone()
}

fn gh(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let out = command.output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "gh not found".to_string(),
        _ => format!("gh: {e}"),
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().next().unwrap_or("gh failed").trim().to_string())
    }
}

fn fetch_lines(root: &Path) -> Vec<String> {
    let pr = match gh(
        root,
        &[
            "pr",
            "view",
            "--json",
            "number,title,state,isDraft,statusCheckRollup,closingIssuesReferences",
        ],
    ) {
        Ok(json) => json,
        Err(e) if e.contains("no pull requests found") => {
            return vec!["(no pull request for this branch)".into()];
        }
        Err(e) => return vec![format!("({e})")],
    };
    let Ok(pr) = serde_json::from_str::<Value>(&pr) else {
        return vec!["(could not read gh output)".into()];
    };
    let issues = pr["closingIssuesReferences"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["number"].as_u64())
        .map(|n| {
            let info = gh(
                root,
                &["issue", "view", &n.to_string(), "--json", "state,title"],
            )
            .ok()
            .and_then(|j| serde_json::from_str::<Value>(&j).ok())
            .unwrap_or(Value::Null);
            (
                n,
                text(&info["state"]).to_lowercase(),
                text(&info["title"]).to_string(),
            )
        })
        .collect::<Vec<_>>();
    lines_for(&pr, &issues)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// Pure formatting of the drawer lines (tested below).
fn lines_for(pr: &Value, issues: &[(u64, String, String)]) -> Vec<String> {
    let state = if pr["isDraft"].as_bool() == Some(true) {
        "draft".to_string()
    } else {
        text(&pr["state"]).to_lowercase()
    };
    let mut lines = vec![format!(
        "#{} · {state} · {}",
        pr["number"].as_u64().unwrap_or(0),
        text(&pr["title"])
    )];
    let checks = pr["statusCheckRollup"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !checks.is_empty() {
        let result = |c: &Value| {
            let s = text(&c["conclusion"]);
            if s.is_empty() {
                text(&c["state"]).to_string()
            } else {
                s.to_string()
            }
        };
        let ok = checks
            .iter()
            .filter(|c| matches!(result(c).as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED"))
            .count();
        let failing = checks
            .iter()
            .filter(|c| {
                matches!(
                    result(c).as_str(),
                    "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
                )
            })
            .count();
        let pending = checks.len() - ok - failing;
        lines.push(format!(
            "  checks: {ok} ok · {failing} failing · {pending} pending"
        ));
    }
    for (n, state, title) in issues {
        lines.push(format!("  closes #{n} · {state} · {title}"));
    }
    lines
}

/// What a drawer line points at.
pub fn parse_ref(line: &str) -> Option<Item> {
    let num = |s: &str| s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok();
    if let Some(rest) = line.strip_prefix('#') {
        return num(rest).map(Item::Pr);
    }
    line.trim_start()
        .strip_prefix("closes #")
        .and_then(num)
        .map(Item::Issue)
}

/// Fetch a PR or issue with its comments and write it as Markdown into the
/// sidebar's private state dir. Blocks on `gh`: called on an explicit click.
pub fn write_markdown(root: &Path, item: Item) -> Result<PathBuf, String> {
    let (cmd, n, label) = match item {
        Item::Pr(n) => ("pr", n, "PR"),
        Item::Issue(n) => ("issue", n, "Issue"),
    };
    let json = gh(
        root,
        &[
            cmd,
            "view",
            &n.to_string(),
            "--json",
            "number,title,state,url,author,body,comments",
        ],
    )?;
    let v: Value = serde_json::from_str(&json).map_err(|e| format!("gh output: {e}"))?;
    let dir = crate::rundir::dir("forge");
    crate::rundir::ensure_private(&dir).map_err(|e| format!("forge dir: {e}"))?;
    let path = dir.join(format!("{cmd}-{n}.md"));
    std::fs::write(&path, markdown(label, &v))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn markdown(label: &str, v: &Value) -> String {
    let mut md = format!(
        "# {label} #{} · {}\n\n**{}** · @{} · {}\n\n{}\n",
        v["number"].as_u64().unwrap_or(0),
        text(&v["title"]),
        text(&v["state"]).to_lowercase(),
        text(&v["author"]["login"]),
        text(&v["url"]),
        text(&v["body"]).trim(),
    );
    for c in v["comments"].as_array().into_iter().flatten() {
        md.push_str(&format!(
            "\n---\n\n**@{}** · {}\n\n{}\n",
            text(&c["author"]["login"]),
            text(&c["createdAt"]).get(..10).unwrap_or(""),
            text(&c["body"]).trim()
        ));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_refs_round_trip() {
        let pr = serde_json::json!({
            "number": 6, "title": "docs: x", "state": "OPEN", "isDraft": false,
            "statusCheckRollup": [{"conclusion": "SUCCESS"}, {"conclusion": "FAILURE"}, {"state": "PENDING"}],
        });
        let lines = lines_for(&pr, &[(3, "open".into(), "PlayOS".into())]);
        assert_eq!(
            lines,
            vec![
                "#6 · open · docs: x",
                "  checks: 1 ok · 1 failing · 1 pending",
                "  closes #3 · open · PlayOS",
            ]
        );
        assert_eq!(parse_ref(&lines[0]), Some(Item::Pr(6)));
        assert_eq!(parse_ref(&lines[1]), None);
        assert_eq!(parse_ref(&lines[2]), Some(Item::Issue(3)));
        assert_eq!(parse_ref("(no pull request for this branch)"), None);
    }

    #[test]
    fn draft_and_markdown() {
        let pr = serde_json::json!({"number": 7, "title": "t", "state": "OPEN", "isDraft": true});
        assert_eq!(lines_for(&pr, &[]), vec!["#7 · draft · t"]);
        let md = markdown(
            "PR",
            &serde_json::json!({
                "number": 7, "title": "t", "state": "OPEN", "url": "u", "author": {"login": "a"},
                "body": "cuerpo", "comments": [{"author": {"login": "b"}, "createdAt": "2026-10-16T00:00:00Z", "body": "ok"}],
            }),
        );
        assert!(md.starts_with("# PR #7 · t\n\n**open** · @a · u\n\ncuerpo\n"));
        assert!(md.contains("**@b** · 2026-10-16\n\nok"));
    }
}
