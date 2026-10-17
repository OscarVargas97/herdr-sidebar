//! The branch's GitHub pull request and the issues it closes, through the
//! `gh` CLI: drawer lines for Source Control and Markdown files the preview
//! pane opens like any other document.
//!
//! `gh` takes a second or more, so drawer lines come from a cache that a
//! background thread refreshes; the heartbeat redraw picks up new results.
//! The task's `work/<branch>/ESTADO.md` (ia-workspace) is a local file and is
//! read on every draw.

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
            let lines = fetch_lines(&key.0, &key.1);
            let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = map.get_mut(&key) {
                entry.lines = lines;
                entry.fetched = Some(Instant::now());
                entry.loading = false;
            }
        });
    }
    let mut lines = estado_lines(root, branch);
    lines.extend(entry.lines.iter().cloned());
    lines
}

fn command(program: &str, root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(program);
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
    command
}

fn run(program: &str, root: &Path, args: &[&str]) -> Result<String, String> {
    let out = command(program, root, args)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{program} not found"),
            _ => format!("{program}: {e}"),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().next().unwrap_or("failed").trim().to_string())
    }
}

fn gh(root: &Path, args: &[&str]) -> Result<String, String> {
    run("gh", root, args)
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

fn fetch_lines(root: &Path, branch: &str) -> Vec<String> {
    let pr = match gh(
        root,
        &[
            "pr",
            "view",
            "--json",
            "number,title,state,isDraft,statusCheckRollup,closingIssuesReferences,reviewDecision",
        ],
    ) {
        Ok(out) => Some(json(&out)),
        Err(e) if e.contains("no pull requests found") => None,
        Err(e) => return vec![format!("({e})")],
    };
    let issues = pr
        .as_ref()
        .and_then(|p| p["closingIssuesReferences"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|i| i["number"].as_u64())
        .map(|n| {
            let info = gh(
                root,
                &["issue", "view", &n.to_string(), "--json", "state,title"],
            )
            .map(|j| json(&j))
            .unwrap_or(Value::Null);
            (
                n,
                text(&info["state"]).to_lowercase(),
                text(&info["title"]).to_string(),
            )
        })
        .collect::<Vec<_>>();
    let threads = pr
        .as_ref()
        .and_then(|p| p["number"].as_u64())
        .and_then(|n| unresolved_threads(root, n));
    let mut lines = lines_for(pr.as_ref(), &issues, threads);
    lines.extend(other_lines(&other_prs(root, branch)));
    lines
}

/// Unresolved review threads of PR `n` (GraphQL: `gh pr view` has no thread state).
fn unresolved_threads(root: &Path, n: u64) -> Option<usize> {
    let query = "query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo)\
                 {pullRequest(number:$n){reviewThreads(first:100){nodes{isResolved}}}}}";
    let out = gh(
        root,
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            "owner={owner}",
            "-F",
            "repo={repo}",
            "-F",
            &format!("n={n}"),
        ],
    )
    .ok()?;
    let nodes = json(&out)["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"].clone();
    Some(
        nodes
            .as_array()?
            .iter()
            .filter(|t| t["isResolved"].as_bool() == Some(false))
            .count(),
    )
}

/// Open PRs whose head is checked out in another worktree of this repo:
/// (number, state, branch, title).
fn other_prs(root: &Path, branch: &str) -> Vec<(u64, String, String, String)> {
    let Ok(porcelain) = run("git", root, &["worktree", "list", "--porcelain"]) else {
        return vec![];
    };
    let branches = worktree_branches(&porcelain);
    let Ok(out) = gh(
        root,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "number,title,headRefName,isDraft",
        ],
    ) else {
        return vec![];
    };
    json(&out)
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            let head = text(&p["headRefName"]);
            head != branch && branches.iter().any(|b| b == head)
        })
        .map(|p| {
            let state = if p["isDraft"].as_bool() == Some(true) {
                "draft"
            } else {
                "open"
            };
            (
                p["number"].as_u64().unwrap_or(0),
                state.to_string(),
                text(&p["headRefName"]).to_string(),
                text(&p["title"]).to_string(),
            )
        })
        .collect()
}

fn worktree_branches(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("branch refs/heads/"))
        .map(str::to_string)
        .collect()
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// Pure formatting of the branch's PR lines (tested below).
fn lines_for(
    pr: Option<&Value>,
    issues: &[(u64, String, String)],
    threads: Option<usize>,
) -> Vec<String> {
    let Some(pr) = pr else {
        return vec!["(no pull request for this branch)".into()];
    };
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
    let decision = text(&pr["reviewDecision"]).to_lowercase().replace('_', " ");
    let unresolved = threads
        .filter(|&n| n > 0)
        .map(|n| format!("{n} unresolved"));
    let review = [(!decision.is_empty()).then_some(decision), unresolved]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if !review.is_empty() {
        lines.push(format!("  review: {}", review.join(" · ")));
    }
    for (n, state, title) in issues {
        lines.push(format!("  closes #{n} · {state} · {title}"));
    }
    lines
}

fn other_lines(others: &[(u64, String, String, String)]) -> Vec<String> {
    others
        .iter()
        .map(|(n, state, branch, title)| format!("  other #{n} · {state} · {branch} · {title}"))
        .collect()
}

/// Anchor and next step from the task's `work/<branch>/ESTADO.md`, looked up
/// in the nearest ancestor of `root` that has one.
fn estado_lines(root: &Path, branch: &str) -> Vec<String> {
    let folder = branch.replace('/', "-");
    root.ancestors()
        .map(|d| d.join("work").join(&folder).join("ESTADO.md"))
        .find(|f| f.is_file())
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| parse_estado(&s))
        .unwrap_or_default()
}

fn parse_estado(md: &str) -> Vec<String> {
    let mut lines = vec![];
    if let Some(anchor) = md.lines().find_map(|l| l.trim().strip_prefix("**Ancla**:")) {
        let anchor = anchor.split(" · **").next().unwrap_or("").trim();
        if !anchor.is_empty() {
            lines.push(format!("estado: {anchor}"));
        }
    }
    let next = md
        .lines()
        .skip_while(|l| !l.trim().eq_ignore_ascii_case("## Próximo paso"))
        .skip(1)
        .map(str::trim)
        .find(|l| !l.is_empty());
    if let Some(next) = next.filter(|l| !l.starts_with('#')) {
        lines.push(format!("next: {next}"));
    }
    lines
}

/// What a drawer line points at.
pub fn parse_ref(line: &str) -> Option<Item> {
    let num = |s: &str| s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok();
    if let Some(rest) = line.strip_prefix('#') {
        return num(rest).map(Item::Pr);
    }
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix("other #") {
        return num(rest).map(Item::Pr);
    }
    line.strip_prefix("closes #").and_then(num).map(Item::Issue)
}

fn cmd_of(item: Item) -> (&'static str, u64, &'static str) {
    match item {
        Item::Pr(n) => ("pr", n, "PR"),
        Item::Issue(n) => ("issue", n, "Issue"),
    }
}

/// Open the PR or issue in the browser (`gh … --web`), without waiting.
pub fn open_web(root: &Path, item: Item) -> Result<(), String> {
    let (cmd, n, _) = cmd_of(item);
    command("gh", root, &[cmd, "view", &n.to_string(), "--web"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("gh: {e}"))
}

/// Fetch a PR or issue with its comments and write it as Markdown into the
/// sidebar's private state dir. Blocks on `gh`: called on an explicit click.
pub fn write_markdown(root: &Path, item: Item) -> Result<PathBuf, String> {
    let (cmd, n, label) = cmd_of(item);
    let out = gh(
        root,
        &[
            cmd,
            "view",
            &n.to_string(),
            "--json",
            "number,title,state,url,author,body,comments",
        ],
    )?;
    let v: Value = serde_json::from_str(&out).map_err(|e| format!("gh output: {e}"))?;
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
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [{"conclusion": "SUCCESS"}, {"conclusion": "FAILURE"}, {"state": "PENDING"}],
        });
        let mut lines = lines_for(Some(&pr), &[(3, "open".into(), "PlayOS".into())], Some(2));
        lines.extend(other_lines(&[(
            9,
            "draft".into(),
            "feat/y".into(),
            "Otra".into(),
        )]));
        assert_eq!(
            lines,
            vec![
                "#6 · open · docs: x",
                "  checks: 1 ok · 1 failing · 1 pending",
                "  review: changes requested · 2 unresolved",
                "  closes #3 · open · PlayOS",
                "  other #9 · draft · feat/y · Otra",
            ]
        );
        assert_eq!(parse_ref(&lines[0]), Some(Item::Pr(6)));
        assert_eq!(parse_ref(&lines[1]), None);
        assert_eq!(parse_ref(&lines[2]), None);
        assert_eq!(parse_ref(&lines[3]), Some(Item::Issue(3)));
        assert_eq!(parse_ref(&lines[4]), Some(Item::Pr(9)));
        assert_eq!(parse_ref("(no pull request for this branch)"), None);
        assert_eq!(parse_ref("next: abrir el PR #4"), None);
    }

    #[test]
    fn no_pr_and_draft() {
        assert_eq!(
            lines_for(None, &[], None),
            vec!["(no pull request for this branch)"]
        );
        let pr = serde_json::json!({"number": 7, "title": "t", "state": "OPEN", "isDraft": true});
        assert_eq!(lines_for(Some(&pr), &[], Some(0)), vec!["#7 · draft · t"]);
    }

    #[test]
    fn estado_anchor_and_next_step() {
        let md = "# feat/x\n\n**Ancla**: solicitud #21 · **PRs**: #14\n\n## Hecho\n- algo\n\n## Próximo paso\n\nAbrir el PR\n\n## Cómo verificar\nx\n";
        assert_eq!(
            parse_estado(md),
            vec!["estado: solicitud #21", "next: Abrir el PR"]
        );
        assert_eq!(parse_estado("# sin nada\n"), Vec::<String>::new());
        let dir = std::env::temp_dir().join(format!("forge-estado-{}", std::process::id()));
        let wt = dir.join("repos").join("app").join("feat-x");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(dir.join("work").join("feat-x")).unwrap();
        std::fs::write(dir.join("work").join("feat-x").join("ESTADO.md"), md).unwrap();
        assert_eq!(estado_lines(&wt, "feat/x").len(), 2);
        assert!(estado_lines(&wt, "feat/otra").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn worktree_branches_from_porcelain() {
        let p = "worktree /a\nHEAD 1\nbranch refs/heads/dev\n\nworktree /b\nHEAD 2\nbranch refs/heads/feat/x\n\nworktree /c\nHEAD 3\ndetached\n";
        assert_eq!(worktree_branches(p), vec!["dev", "feat/x"]);
    }

    #[test]
    fn markdown_has_body_and_comments() {
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
