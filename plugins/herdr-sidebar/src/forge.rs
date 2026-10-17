//! GitHub through the `gh` CLI for the sidebar's GitHub view: the space's
//! works, open pull requests, open issues with their sub-issues, and the PR /
//! issue cards the preview pane opens as Markdown.
//!
//! `gh` takes a second or more, so the PR and issue lists come from a cache
//! that a background thread refreshes; the heartbeat redraw picks up new
//! results. Works are local (`work/*/ESTADO.md`, checkouts, herdr panes) and
//! rebuilt on every refresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a fetched list is shown before the next background refresh.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// What a line opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// A pull request; `gh` runs from this checkout of its repo.
    Pr(PathBuf, u64),
    /// An issue; `gh` runs from this checkout of its repo.
    Issue(PathBuf, u64),
    /// A local file, e.g. a work's ESTADO.md.
    File(PathBuf),
}

/// How a line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// A repo name above its items.
    Group,
    /// The item itself (PR, issue, work).
    Head,
    /// A work with an agent working in it right now.
    Working,
    /// Details under an item.
    Meta,
    /// Loading, empty or error notes.
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub tone: Tone,
    pub link: Option<Link>,
}

fn item(text: impl Into<String>, tone: Tone, link: Option<Link>) -> Item {
    Item {
        text: text.into(),
        tone,
        link,
    }
}

fn note(text: impl Into<String>) -> Item {
    item(text, Tone::Note, None)
}

// ---- gh / git ----

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

/// One GraphQL query against the repo checked out at `root`.
fn graphql(root: &Path, query: &str, n: Option<u64>) -> Result<Value, String> {
    let q = format!("query={query}");
    let n = n.map(|n| format!("n={n}"));
    let mut args = vec![
        "api",
        "graphql",
        "-f",
        &q,
        "-F",
        "owner={owner}",
        "-F",
        "repo={repo}",
    ];
    if let Some(n) = &n {
        args.extend(["-F", n]);
    }
    let v = json(&gh(root, &args)?);
    match v["errors"][0]["message"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(v["data"]["repository"].clone()),
    }
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn num(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// `owner/repo` of a GitHub remote URL.
fn slug_of(url: &str) -> Option<String> {
    let rest = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = &rest[rest.find("github.com")? + "github.com".len() + 1..];
    let mut parts = rest.split('/');
    match (parts.next(), parts.next()) {
        (Some(o), Some(r)) if !o.is_empty() && !r.is_empty() => Some(format!("{o}/{r}")),
        _ => None,
    }
}

/// One checkout per GitHub repo among `roots`: (owner/repo, checkout).
fn repos(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    static SLUGS: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    let mut slugs = SLUGS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for root in roots {
        let slug = slugs.entry(root.clone()).or_insert_with(|| {
            run("git", root, &["remote", "get-url", "origin"])
                .ok()
                .and_then(|u| slug_of(&u))
        });
        if let Some(slug) = slug
            && !out.iter().any(|(s, _)| s == slug)
        {
            out.push((slug.clone(), root.clone()));
        }
    }
    out
}

// ---- background cache ----

struct Cached {
    value: Option<Value>,
    fetched: Option<Instant>,
    loading: bool,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The cached value for `key` (`None` while the first fetch runs); starts a
/// background `fetch` when it is missing or stale.
fn cached(key: String, fetch: impl FnOnce() -> Value + Send + 'static) -> Option<Value> {
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.entry(key.clone()).or_insert(Cached {
        value: None,
        fetched: None,
        loading: false,
    });
    let stale = entry.fetched.is_none_or(|t| t.elapsed() >= FRESH_FOR);
    if stale && !entry.loading {
        entry.loading = true;
        std::thread::spawn(move || {
            let value = fetch();
            let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = map.get_mut(&key) {
                entry.value = Some(value);
                entry.fetched = Some(Instant::now());
                entry.loading = false;
            }
        });
    }
    entry.value.clone()
}

/// Make every list refetch on its next read (the `r` key).
pub fn invalidate() {
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    for entry in map.values_mut() {
        entry.fetched = None;
    }
}

/// Run `per_repo` for each repo; errors are kept per repo.
fn fetch_each(
    repos: Vec<(String, PathBuf)>,
    field: &'static str,
    per_repo: fn(&Path) -> Result<Value, String>,
) -> Value {
    // One thread per repo: `gh` round trips dominate.
    std::thread::scope(|scope| {
        let handles: Vec<_> = repos
            .into_iter()
            .map(|(slug, root)| {
                scope.spawn(move || {
                    let mut v = json!({"slug": slug, "root": root.to_string_lossy()});
                    match per_repo(&root) {
                        Ok(data) => v[field] = data,
                        Err(e) => v["error"] = Value::String(e),
                    }
                    v
                })
            })
            .collect();
        Value::Array(handles.into_iter().filter_map(|h| h.join().ok()).collect())
    })
}

fn key(kind: &str, repos: &[(String, PathBuf)]) -> String {
    let slugs: Vec<&str> = repos.iter().map(|(s, _)| s.as_str()).collect();
    format!("{kind}:{}", slugs.join(","))
}

// ---- pull requests ----

fn fetch_prs(root: &Path) -> Result<Value, String> {
    gh(
        root,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "50",
            "--json",
            "number,title,isDraft,headRefName,baseRefName,statusCheckRollup,reviewDecision",
        ],
    )
    .map(|out| json(&out))
}

fn pulls_data(roots: &[PathBuf]) -> Option<Value> {
    let repos = repos(roots);
    let key = key("pulls", &repos);
    cached(key, move || fetch_each(repos, "prs", fetch_prs))
}

/// The Pull Requests section.
pub fn pulls(roots: &[PathBuf]) -> Vec<Item> {
    match pulls_data(roots) {
        Some(data) => pull_items(&data),
        None => vec![note("cargando…")],
    }
}

fn pull_items(data: &Value) -> Vec<Item> {
    let mut out = pull_items_inner(data);
    if out.is_empty() {
        out.push(note("sin PRs abiertos"));
    }
    out
}

fn pull_items_inner(data: &Value) -> Vec<Item> {
    let repos = list(data);
    let mut out = Vec::new();
    for repo in repos {
        let root = PathBuf::from(text(&repo["root"]));
        let prs = list(&repo["prs"]);
        if repos.len() > 1 && prs.is_empty() && repo["error"].is_null() {
            continue;
        }
        if repos.len() > 1 {
            out.push(item(short(text(&repo["slug"])), Tone::Group, None));
        }
        if let Some(e) = repo["error"].as_str() {
            out.push(note(format!("({e})")));
            continue;
        }
        for p in prs {
            let link = Some(Link::Pr(root.clone(), num(&p["number"])));
            out.push(item(
                format!("#{} {}", num(&p["number"]), text(&p["title"])),
                Tone::Head,
                link.clone(),
            ));
            let mut meta = vec![format!(
                "{} → {}",
                text(&p["headRefName"]),
                text(&p["baseRefName"])
            )];
            if p["isDraft"].as_bool() == Some(true) {
                meta.insert(0, "borrador".into());
            }
            meta.extend(checks(&p["statusCheckRollup"]));
            meta.extend(review(text(&p["reviewDecision"])).map(String::from));
            out.push(item(format!("  {}", meta.join(" · ")), Tone::Meta, link));
        }
    }
    out
}

fn short(slug: &str) -> &str {
    slug.rsplit('/').next().unwrap_or(slug)
}

/// `✓3 ✗1 …2` from a status-check rollup (`gh` JSON or GraphQL contexts).
fn checks(rollup: &Value) -> Option<String> {
    let (ok, failing, pending) = check_counts(list(rollup));
    let parts: Vec<String> = [("✓", ok), ("✗", failing), ("…", pending)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(g, n)| format!("{g}{n}"))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn check_result(c: &Value) -> &str {
    [&c["conclusion"], &c["state"]]
        .into_iter()
        .map(text)
        .find(|s| !s.is_empty())
        .unwrap_or("PENDING")
}

fn check_counts(checks: &[Value]) -> (usize, usize, usize) {
    let ok = checks
        .iter()
        .filter(|c| matches!(check_result(c), "SUCCESS" | "NEUTRAL" | "SKIPPED"))
        .count();
    let failing = checks
        .iter()
        .filter(|c| {
            matches!(
                check_result(c),
                "FAILURE"
                    | "ERROR"
                    | "TIMED_OUT"
                    | "CANCELLED"
                    | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE"
            )
        })
        .count();
    (ok, failing, checks.len() - ok - failing)
}

fn review(decision: &str) -> Option<&'static str> {
    match decision {
        "APPROVED" => Some("aprobado"),
        "CHANGES_REQUESTED" => Some("cambios pedidos"),
        "REVIEW_REQUIRED" => Some("falta revisión"),
        _ => None,
    }
}

// ---- issues ----

const ISSUES_QUERY: &str = "query($owner:String!,$repo:String!){repository(owner:$owner,name:$repo){\
    issues(states:OPEN,first:100,orderBy:{field:CREATED_AT,direction:ASC}){nodes{\
    number title milestone{title} parent{number} subIssuesSummary{total completed}}}}}";

fn fetch_issues(root: &Path) -> Result<Value, String> {
    graphql(root, ISSUES_QUERY, None).map(|r| r["issues"]["nodes"].clone())
}

/// The Issues section: each request with its open sub-issues nested.
pub fn issues(roots: &[PathBuf]) -> Vec<Item> {
    let repos = repos(roots);
    let key = key("issues", &repos);
    match cached(key, move || fetch_each(repos, "issues", fetch_issues)) {
        Some(data) => issue_items(&data),
        None => vec![note("cargando…")],
    }
}

fn issue_items(data: &Value) -> Vec<Item> {
    let mut out = issue_items_inner(data);
    if out.is_empty() {
        out.push(note("sin issues abiertos"));
    }
    out
}

fn issue_items_inner(data: &Value) -> Vec<Item> {
    let repos = list(data);
    let mut out = Vec::new();
    for repo in repos {
        let root = PathBuf::from(text(&repo["root"]));
        let nodes = list(&repo["issues"]);
        if repos.len() > 1 && nodes.is_empty() && repo["error"].is_null() {
            continue;
        }
        if repos.len() > 1 {
            out.push(item(short(text(&repo["slug"])), Tone::Group, None));
        }
        if let Some(e) = repo["error"].as_str() {
            out.push(note(format!("({e})")));
            continue;
        }
        let open = |n: u64| nodes.iter().any(|i| num(&i["number"]) == n);
        let mut tops: Vec<&Value> = nodes
            .iter()
            .filter(|i| !open(num(&i["parent"]["number"])))
            .collect();
        tops.sort_by_key(|i| num(&i["number"]));
        for top in tops {
            push_issue(&mut out, nodes, top, 0, &root);
        }
    }
    out
}

fn push_issue(out: &mut Vec<Item>, nodes: &[Value], issue: &Value, depth: usize, root: &Path) {
    let n = num(&issue["number"]);
    let summary = &issue["subIssuesSummary"];
    let progress = if num(&summary["total"]) > 0 {
        format!(
            " · {}/{}",
            num(&summary["completed"]),
            num(&summary["total"])
        )
    } else {
        String::new()
    };
    let indent = if depth == 0 {
        String::new()
    } else {
        format!("{}↳ ", "  ".repeat(depth))
    };
    out.push(item(
        format!("{indent}#{n} {}{progress}", text(&issue["title"])),
        Tone::Head,
        Some(Link::Issue(root.to_path_buf(), n)),
    ));
    let mut children: Vec<&Value> = nodes
        .iter()
        .filter(|i| num(&i["parent"]["number"]) == n)
        .collect();
    children.sort_by_key(|i| num(&i["number"]));
    for child in children {
        push_issue(out, nodes, child, depth + 1, root);
    }
}

// ---- works ----

/// The space folder (ia-workspace: `.base/` + `work/`) that holds `cwd`.
fn space_of(cwd: &Path) -> Option<&Path> {
    cwd.ancestors()
        .find(|d| d.join(".base").exists() && d.join("work").is_dir())
}

/// The checkouts of the space that holds `cwd` (`repos/<repo>/<checkout>`
/// and `docs/<checkout>`), with their branch — one level deeper than the
/// Source Control scan reaches. Empty outside a space.
pub fn space_checkouts(cwd: &Path) -> Vec<(PathBuf, String)> {
    let Some(space) = space_of(cwd) else {
        return vec![];
    };
    let subdirs = |d: PathBuf| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v
    };
    subdirs(space.join("repos"))
        .into_iter()
        .flat_map(subdirs)
        .chain(subdirs(space.join("docs")))
        .filter_map(|dir| head_branch(&dir).map(|b| (dir, b)))
        .collect()
}

/// The branch checked out at `dir`, read from its HEAD file (no git process).
fn head_branch(dir: &Path) -> Option<String> {
    let dot = dir.join(".git");
    let gitdir = if dot.is_dir() {
        dot
    } else {
        let line = std::fs::read_to_string(&dot).ok()?;
        let target = PathBuf::from(line.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            dir.join(target)
        }
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    Some(
        head.trim()
            .strip_prefix("ref: refs/heads/")
            .unwrap_or("(detached)")
            .to_string(),
    )
}

/// Markdown links and code spans as plain text, for one-line display.
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        let Some(mid) = rest[start..].find("](") else {
            break;
        };
        let Some(end) = rest[start + mid..].find(')') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&rest[start + 1..start + mid]);
        rest = &rest[start + mid + end + 1..];
    }
    out.push_str(rest);
    out.replace('`', "")
}

/// Folder name of a branch under `work/` (ws.py `folder`).
fn folder(branch: &str) -> String {
    branch.replace('/', "-")
}

/// The Works section: one entry per work of the space (or per checkout
/// outside a space) with its anchor, PR, next step and agent state.
/// `checkouts` are (checkout root, current branch).
pub fn works(cwd: &Path, checkouts: &[(PathBuf, String)]) -> Vec<Item> {
    let roots: Vec<PathBuf> = checkouts.iter().map(|(r, _)| r.clone()).collect();
    let prs = pulls_data(&roots).unwrap_or(Value::Null);
    let agents = agents();
    // (label, branch, ESTADO.md, checkouts)
    let entries: Vec<(String, String, Option<PathBuf>, Vec<PathBuf>)> = match space_of(cwd) {
        Some(space) => {
            let mut dirs: Vec<_> = std::fs::read_dir(space.join("work"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with(['_', '.']))
                .collect();
            dirs.sort();
            dirs.into_iter()
                .map(|name| {
                    let roots = checkouts
                        .iter()
                        .filter(|(_, b)| folder(b) == name)
                        .map(|(r, _)| r.clone())
                        .collect::<Vec<_>>();
                    let branch = checkouts
                        .iter()
                        .find(|(_, b)| folder(b) == name)
                        .map_or(name.clone(), |(_, b)| b.clone());
                    let estado = space.join("work").join(&name).join("ESTADO.md");
                    (
                        branch.clone(),
                        branch,
                        estado.is_file().then_some(estado),
                        roots,
                    )
                })
                .collect()
        }
        None => checkouts
            .iter()
            .map(|(root, branch)| {
                let estado = root
                    .ancestors()
                    .map(|d| d.join("work").join(folder(branch)).join("ESTADO.md"))
                    .find(|f| f.is_file());
                let repo = root.file_name().map(|n| n.to_string_lossy().into_owned());
                let label = format!("{}: {branch}", repo.unwrap_or_default());
                (label, branch.clone(), estado, vec![root.clone()])
            })
            .collect(),
    };
    if entries.is_empty() {
        return vec![note("sin works: make task crea uno")];
    }
    let mut out = Vec::new();
    let outside = space_of(cwd).is_none();
    for (label, branch, estado, roots) in entries {
        let md = estado
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .unwrap_or_default();
        let (anchor, next) = parse_estado(&md);
        let agent = roots
            .iter()
            .filter_map(|r| agent_in(&agents, r))
            .max_by_key(|(_, status)| status == "working");
        let pr = pr_for(&prs, &branch);
        // Outside a space every repo is a checkout: list only the busy ones.
        let base = matches!(branch.as_str(), "" | "main" | "master" | "dev" | "develop");
        if outside && base && estado.is_none() && agent.is_none() && pr.is_none() {
            continue;
        }
        out.extend(work_items(&label, estado, &roots, anchor, next, agent, pr));
    }
    if out.is_empty() {
        out.push(note("sin works en curso"));
    }
    out
}

/// Pure formatting of one work (tested below).
fn work_items(
    branch: &str,
    estado: Option<PathBuf>,
    roots: &[PathBuf],
    anchor: Option<String>,
    next: Option<String>,
    agent: Option<(String, String)>,
    pr: Option<(PathBuf, Value)>,
) -> Vec<Item> {
    let mut out = Vec::new();
    let (tone, agent_text) = match &agent {
        Some((name, status)) => (
            if status == "working" {
                Tone::Working
            } else {
                Tone::Head
            },
            format!(" · {name} {}", agent_status(status)),
        ),
        None => (Tone::Head, String::new()),
    };
    out.push(item(
        format!("● {branch}{agent_text}"),
        tone,
        estado.map(Link::File),
    ));
    if let Some(anchor) = anchor {
        let link = roots
            .first()
            .zip(first_number(&anchor))
            .map(|(r, n)| Link::Issue(r.clone(), n));
        out.push(item(format!("  {anchor}"), Tone::Meta, link));
    }
    if let Some((root, p)) = pr {
        let mut meta = vec![format!("PR #{}", num(&p["number"]))];
        meta.extend(checks(&p["statusCheckRollup"]));
        meta.extend(review(text(&p["reviewDecision"])).map(String::from));
        out.push(item(
            format!("  {}", meta.join(" · ")),
            Tone::Meta,
            Some(Link::Pr(root, num(&p["number"]))),
        ));
    }
    if roots.is_empty() {
        out.push(note("  sin worktree"));
    }
    if let Some(next) = next {
        out.push(item(format!("  → {next}"), Tone::Meta, None));
    }
    out
}

fn agent_status(status: &str) -> &str {
    match status {
        "working" => "trabajando",
        "blocked" | "waiting" => "espera respuesta",
        "done" => "terminó",
        "idle" => "en espera",
        other => other,
    }
}

fn first_number(s: &str) -> Option<u64> {
    let rest = &s[s.find('#')? + 1..];
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// The open PR whose head is `branch`, with the checkout to run `gh` from.
fn pr_for(data: &Value, branch: &str) -> Option<(PathBuf, Value)> {
    list(data).iter().find_map(|repo| {
        list(&repo["prs"])
            .iter()
            .find(|p| text(&p["headRefName"]) == branch)
            .map(|p| (PathBuf::from(text(&repo["root"])), p.clone()))
    })
}

/// herdr panes with an agent: (cwd, agent name, status).
fn agents() -> Vec<(String, String, String)> {
    let Ok(out) = crate::ipc::call_text("pane.list", json!({})) else {
        return vec![];
    };
    let v = json(&out);
    let panes = if v["result"]["panes"].is_array() {
        &v["result"]["panes"]
    } else {
        &v["panes"]
    };
    list(panes)
        .iter()
        .filter(|p| !text(&p["agent"]).is_empty())
        .map(|p| {
            (
                norm(text(&p["cwd"])),
                text(&p["agent"]).to_string(),
                text(&p["agent_status"]).to_string(),
            )
        })
        .collect()
}

fn norm(path: &str) -> String {
    let p = path.replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) { p.to_lowercase() } else { p }
}

fn agent_in(agents: &[(String, String, String)], root: &Path) -> Option<(String, String)> {
    let root = norm(&root.to_string_lossy());
    agents
        .iter()
        .filter(|(cwd, _, _)| *cwd == root || cwd.starts_with(&format!("{root}/")))
        .map(|(_, name, status)| (name.clone(), status.clone()))
        .max_by_key(|(_, status)| status == "working")
}

/// Anchor and next step of an ESTADO.md.
fn parse_estado(md: &str) -> (Option<String>, Option<String>) {
    let anchor = md
        .lines()
        .find_map(|l| l.trim().strip_prefix("**Ancla**:"))
        .map(|a| plain(a.split(" · **").next().unwrap_or("").trim()))
        .filter(|a| !a.is_empty() && !a.starts_with('<'));
    let next = md
        .lines()
        .skip_while(|l| !l.trim().eq_ignore_ascii_case("## Próximo paso"))
        .skip(1)
        .map(str::trim)
        .find(|l| !l.is_empty())
        .filter(|l| !l.starts_with('#') && !l.starts_with('<'))
        .map(|l| plain(l.trim_start_matches(['-', '*', ' '])));
    (anchor, next)
}

// ---- cards ----

/// Open a PR or issue in the browser (`gh … --web`), without waiting.
pub fn open_web(link: &Link) -> Result<(), String> {
    let (cmd, root, n) = match link {
        Link::Pr(root, n) => ("pr", root, n),
        Link::Issue(root, n) => ("issue", root, n),
        Link::File(_) => return Err("not on GitHub".into()),
    };
    command("gh", root, &[cmd, "view", &n.to_string(), "--web"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("gh: {e}"))
}

const PR_QUERY: &str = "query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo){\
    pullRequest(number:$n){number title state isDraft url author{login} body baseRefName headRefName \
    additions deletions reviewDecision \
    comments(first:100){nodes{author{login __typename} body createdAt}} \
    reviews(first:50){nodes{author{login __typename} state body submittedAt}} \
    reviewThreads(first:100){nodes{isResolved path line comments(first:20){nodes{author{login} body}}}} \
    files(first:100){nodes{path additions deletions changeType}} \
    closingIssuesReferences(first:20){nodes{number title state}} \
    commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{__typename \
    ... on CheckRun{name conclusion status} ... on StatusContext{context state}}}}}}}}}}";

const ISSUE_QUERY: &str = "query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo){\
    issue(number:$n){number title state url author{login} body milestone{title} \
    labels(first:20){nodes{name}} \
    comments(first:100){nodes{author{login __typename} body createdAt}} \
    parent{number title state} subIssuesSummary{total completed} \
    subIssues(first:50){nodes{number title state}}}}}";

/// Fetch a PR or issue card and write it as Markdown into the sidebar's
/// private state dir. Blocks on `gh`: run it off the UI thread. A file link
/// is returned as is. A failed fetch still writes a card with the error.
pub fn write_markdown(link: &Link) -> Result<PathBuf, String> {
    let (name, md) = match link {
        Link::File(path) => return Ok(path.clone()),
        Link::Pr(root, n) => (
            format!("pr-{n}.md"),
            graphql(root, PR_QUERY, Some(*n))
                .map(|r| pr_card(&r["pullRequest"]))
                .unwrap_or_else(|e| format!("# PR #{n}\n\nNo se pudo cargar: {e}\n")),
        ),
        Link::Issue(root, n) => (
            format!("issue-{n}.md"),
            graphql(root, ISSUE_QUERY, Some(*n))
                .map(|r| issue_card(&r["issue"]))
                .unwrap_or_else(|e| format!("# Issue #{n}\n\nNo se pudo cargar: {e}\n")),
        ),
    };
    let dir = crate::rundir::dir("forge");
    crate::rundir::ensure_private(&dir).map_err(|e| format!("forge dir: {e}"))?;
    let path = dir.join(name);
    std::fs::write(&path, md).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn is_bot(author: &Value) -> bool {
    text(&author["__typename"]) == "Bot" || text(&author["login"]).ends_with("[bot]")
}

fn state_es(state: &str, draft: bool) -> &'static str {
    match (state, draft) {
        (_, true) => "borrador",
        ("OPEN", _) => "abierto",
        ("CLOSED", _) => "cerrado",
        ("MERGED", _) => "fusionado",
        _ => "?",
    }
}

fn date(v: &Value) -> &str {
    text(v).get(..10).unwrap_or("")
}

fn body_or(v: &Value, empty: &str) -> String {
    let b = body(v);
    if b.is_empty() {
        format!("_{empty}_")
    } else {
        b
    }
}

/// A user-written Markdown body, its headings pushed two levels down so they
/// nest under the card's own sections (code fences left alone).
fn body(v: &Value) -> String {
    let mut fenced = false;
    text(v)
        .trim()
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("```") {
                fenced = !fenced;
            }
            if !fenced && l.starts_with('#') {
                format!("##{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

/// Pure Markdown of a PR card (tested below).
fn pr_card(pr: &Value) -> String {
    let draft = pr["isDraft"].as_bool() == Some(true);
    let mut md = format!(
        "# #{} · {}\n\n**{}** · `{}` → `{}` · @{} · +{} −{}\n\n",
        num(&pr["number"]),
        text(&pr["title"]),
        state_es(text(&pr["state"]), draft),
        text(&pr["headRefName"]),
        text(&pr["baseRefName"]),
        text(&pr["author"]["login"]),
        num(&pr["additions"]),
        num(&pr["deletions"]),
    );
    let contexts =
        pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"].clone();
    let contexts = list(&contexts);
    if !contexts.is_empty() {
        let (ok, failing, pending) = check_counts(contexts);
        let failed: Vec<&str> = contexts
            .iter()
            .filter(|c| {
                !matches!(
                    check_result(c),
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" | "PENDING" | "IN_PROGRESS" | "QUEUED"
                )
            })
            .map(|c| {
                [text(&c["name"]), text(&c["context"])]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or("?")
            })
            .collect();
        let mut line = format!("- **Checks**: ✓ {ok} · ✗ {failing} · … {pending}");
        if !failed.is_empty() {
            line.push_str(&format!(" (falla: {})", failed.join(", ")));
        }
        md.push_str(&line);
        md.push('\n');
    }
    let threads: Vec<&Value> = list(&pr["reviewThreads"]["nodes"])
        .iter()
        .filter(|t| t["isResolved"].as_bool() == Some(false))
        .collect();
    let mut rev = review(text(&pr["reviewDecision"]))
        .unwrap_or("sin revisión")
        .to_string();
    if !threads.is_empty() {
        rev.push_str(&format!(" · {} hilos sin resolver", threads.len()));
    }
    md.push_str(&format!("- **Revisión**: {rev}\n"));
    for i in list(&pr["closingIssuesReferences"]["nodes"]) {
        md.push_str(&format!(
            "- **Cierra**: #{} {} ({})\n",
            num(&i["number"]),
            text(&i["title"]),
            state_es(text(&i["state"]), false)
        ));
    }
    md.push_str(&format!("- **Enlace**: {}\n\n", text(&pr["url"])));

    // Conversation: comments and reviews with something to say, oldest first.
    let mut talk: Vec<(String, String, String)> = list(&pr["comments"]["nodes"])
        .iter()
        .filter(|c| !is_bot(&c["author"]))
        .map(|c| {
            (
                text(&c["createdAt"]).to_string(),
                format!(
                    "@{} · {}",
                    text(&c["author"]["login"]),
                    date(&c["createdAt"])
                ),
                body(&c["body"]),
            )
        })
        .collect();
    for r in list(&pr["reviews"]["nodes"])
        .iter()
        .filter(|r| !is_bot(&r["author"]))
    {
        let verdict = match text(&r["state"]) {
            "APPROVED" => " · aprobó",
            "CHANGES_REQUESTED" => " · pidió cambios",
            _ => "",
        };
        let body = body(&r["body"]);
        if verdict.is_empty() && body.is_empty() {
            continue;
        }
        talk.push((
            text(&r["submittedAt"]).to_string(),
            format!(
                "@{} · {}{verdict}",
                text(&r["author"]["login"]),
                date(&r["submittedAt"])
            ),
            body,
        ));
    }
    talk.sort_by(|a, b| a.0.cmp(&b.0));
    let files = list(&pr["files"]["nodes"]);
    md.push_str(&format!(
        "> Descripción · Conversación ({}) · Hilos sin resolver ({}) · Archivos ({})\n\n",
        talk.len(),
        threads.len(),
        files.len()
    ));
    md.push_str(&format!(
        "## Descripción\n\n{}\n\n",
        body_or(&pr["body"], "sin descripción")
    ));
    md.push_str(&format!("## Conversación ({})\n\n", talk.len()));
    for (_, head, body) in &talk {
        md.push_str(&format!("### {head}\n\n{body}\n\n"));
    }
    md.push_str(&format!("## Hilos sin resolver ({})\n\n", threads.len()));
    for t in &threads {
        let at = match t["line"].as_u64() {
            Some(l) => format!("{}:{l}", text(&t["path"])),
            None => text(&t["path"]).to_string(),
        };
        md.push_str(&format!("### `{at}`\n\n"));
        for c in list(&t["comments"]["nodes"]) {
            md.push_str(&format!(
                "**@{}**: {}\n\n",
                text(&c["author"]["login"]),
                body(&c["body"])
            ));
        }
    }
    md.push_str(&format!("## Archivos ({})\n\n", files.len()));
    for f in files {
        let kind = match text(&f["changeType"]) {
            "ADDED" => "A",
            "DELETED" => "D",
            "RENAMED" => "R",
            _ => "M",
        };
        md.push_str(&format!(
            "- {kind} `{}` +{} −{}\n",
            text(&f["path"]),
            num(&f["additions"]),
            num(&f["deletions"])
        ));
    }
    md
}

/// Pure Markdown of an issue card (tested below).
fn issue_card(issue: &Value) -> String {
    let mut md = format!(
        "# #{} · {}\n\n**{}** · @{}\n\n",
        num(&issue["number"]),
        text(&issue["title"]),
        state_es(text(&issue["state"]), false),
        text(&issue["author"]["login"]),
    );
    if let Some(m) = issue["milestone"]["title"].as_str() {
        md.push_str(&format!("- **Milestone**: {m}\n"));
    }
    let labels: Vec<&str> = list(&issue["labels"]["nodes"])
        .iter()
        .map(|l| text(&l["name"]))
        .collect();
    if !labels.is_empty() {
        md.push_str(&format!("- **Etiquetas**: {}\n", labels.join(", ")));
    }
    if issue["parent"].is_object() {
        md.push_str(&format!(
            "- **Parte de**: #{} {}\n",
            num(&issue["parent"]["number"]),
            text(&issue["parent"]["title"])
        ));
    }
    md.push_str(&format!("- **Enlace**: {}\n\n", text(&issue["url"])));
    let subs = list(&issue["subIssues"]["nodes"]);
    let comments: Vec<&Value> = list(&issue["comments"]["nodes"])
        .iter()
        .filter(|c| !is_bot(&c["author"]))
        .collect();
    let mut index = vec!["Descripción".to_string()];
    if !subs.is_empty() {
        index.push(format!(
            "Tareas ({}/{})",
            num(&issue["subIssuesSummary"]["completed"]),
            num(&issue["subIssuesSummary"]["total"])
        ));
    }
    index.push(format!("Conversación ({})", comments.len()));
    md.push_str(&format!("> {}\n\n", index.join(" · ")));
    md.push_str(&format!(
        "## Descripción\n\n{}\n\n",
        body_or(&issue["body"], "sin descripción")
    ));
    if !subs.is_empty() {
        md.push_str(&format!("## {}\n\n", index[1]));
        for s in subs {
            let mark = if text(&s["state"]) == "CLOSED" {
                "x"
            } else {
                " "
            };
            md.push_str(&format!(
                "- [{mark}] #{} {}\n",
                num(&s["number"]),
                text(&s["title"])
            ));
        }
        md.push('\n');
    }
    md.push_str(&format!("## Conversación ({})\n\n", comments.len()));
    for c in comments {
        md.push_str(&format!(
            "### @{} · {}\n\n{}\n\n",
            text(&c["author"]["login"]),
            date(&c["createdAt"]),
            body(&c["body"])
        ));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_from_remote_urls() {
        assert_eq!(
            slug_of("https://github.com/o/r.git\n").as_deref(),
            Some("o/r")
        );
        assert_eq!(slug_of("git@github.com:o/r.git").as_deref(), Some("o/r"));
        assert_eq!(slug_of("https://gitlab.com/o/r"), None);
        assert_eq!(slug_of("D:/local/repo"), None);
    }

    #[test]
    fn pull_items_group_by_repo_and_summarize() {
        let data = json!([
            {"slug": "o/a", "root": "/a", "prs": [{
                "number": 6, "title": "docs: x", "isDraft": false, "headRefName": "feat/x",
                "baseRefName": "dev", "reviewDecision": "CHANGES_REQUESTED",
                "statusCheckRollup": [{"conclusion": "SUCCESS"}, {"conclusion": "FAILURE"}, {"state": "PENDING"}]
            }]},
            {"slug": "o/b", "root": "/b", "error": "HTTP 401"}
        ]);
        let items = pull_items(&data);
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "a",
                "#6 docs: x",
                "  feat/x → dev · ✓1 ✗1 …1 · cambios pedidos",
                "b",
                "(HTTP 401)"
            ]
        );
        assert_eq!(items[1].link, Some(Link::Pr("/a".into(), 6)));
        assert_eq!(items[2].link, items[1].link);
        assert_eq!(items[0].tone, Tone::Group);
        assert_eq!(
            pull_items(&json!([{"slug": "o/a", "root": "/a", "prs": []}]))[0].text,
            "sin PRs abiertos"
        );
    }

    #[test]
    fn issues_nest_under_their_request() {
        let data = json!([{"slug": "o/a", "root": "/a", "issues": [
            {"number": 8, "title": "P0.2", "parent": {"number": 7}},
            {"number": 4, "title": "P0.1", "parent": {"number": 7}},
            {"number": 7, "title": "P0", "parent": null, "subIssuesSummary": {"total": 7, "completed": 1}},
            {"number": 2, "title": "suelto", "parent": {"number": 1}},
        ]}]);
        let texts: Vec<String> = issue_items(&data).into_iter().map(|i| i.text).collect();
        assert_eq!(
            texts,
            vec!["#2 suelto", "#7 P0 · 1/7", "  ↳ #4 P0.1", "  ↳ #8 P0.2"]
        );
    }

    #[test]
    fn work_shows_agent_anchor_pr_and_next_step() {
        let md = "# feat/x\n\n**Ancla**: solicitud #21 · **PRs**: #14\n\n## Próximo paso\n\nAbrir el PR\n";
        let (anchor, next) = parse_estado(md);
        assert_eq!(anchor.as_deref(), Some("solicitud #21"));
        assert_eq!(next.as_deref(), Some("Abrir el PR"));
        assert_eq!(
            parse_estado("**Ancla**: <issue #n>\n## Próximo paso\n<una acción>\n"),
            (None, None)
        );
        assert_eq!(
            parse_estado(
                "**Ancla**: milestone [P0](https://x/2) (a)\n## Próximo paso\n- a#4: en `repos/a`.\n"
            ),
            (
                Some("milestone P0 (a)".into()),
                Some("a#4: en repos/a.".into())
            )
        );
        let items = work_items(
            "feat/x",
            Some("/w/ESTADO.md".into()),
            &["/r".into()],
            anchor,
            next,
            Some(("claude".into(), "working".into())),
            Some((
                "/r".into(),
                json!({"number": 14, "statusCheckRollup": [{"conclusion": "SUCCESS"}]}),
            )),
        );
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "● feat/x · claude trabajando",
                "  solicitud #21",
                "  PR #14 · ✓1",
                "  → Abrir el PR"
            ]
        );
        assert_eq!(items[0].tone, Tone::Working);
        assert_eq!(items[0].link, Some(Link::File("/w/ESTADO.md".into())));
        assert_eq!(items[1].link, Some(Link::Issue("/r".into(), 21)));
        assert_eq!(items[2].link, Some(Link::Pr("/r".into(), 14)));
        let bare = work_items("feat/y", None, &[], None, None, None, None);
        assert_eq!(
            bare.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            vec!["● feat/y", "  sin worktree"]
        );
    }

    #[test]
    fn agents_match_inside_the_checkout_only() {
        let agents = vec![
            (
                norm("D:\\s\\repos\\a\\feat-x\\src"),
                "claude".into(),
                "done".into(),
            ),
            (
                norm("D:\\s\\repos\\a\\feat-xy"),
                "codex".into(),
                "working".into(),
            ),
        ];
        assert_eq!(
            agent_in(&agents, Path::new("D:\\s\\repos\\a\\feat-x")),
            Some(("claude".into(), "done".into()))
        );
    }

    #[test]
    fn pr_card_has_header_index_and_sections_without_bots() {
        let pr = json!({
            "number": 6, "title": "t", "state": "OPEN", "isDraft": false, "url": "u",
            "author": {"login": "a"}, "body": "", "baseRefName": "dev", "headRefName": "feat/x",
            "additions": 3, "deletions": 1, "reviewDecision": "APPROVED",
            "comments": {"nodes": [
                {"author": {"login": "b", "__typename": "User"}, "body": "ok", "createdAt": "2026-10-16T00:00:00Z"},
                {"author": {"login": "ci", "__typename": "Bot"}, "body": "ruido", "createdAt": "2026-10-15T00:00:00Z"}
            ]},
            "reviews": {"nodes": [{"author": {"login": "c"}, "state": "APPROVED", "body": "", "submittedAt": "2026-10-17T00:00:00Z"}]},
            "reviewThreads": {"nodes": [
                {"isResolved": false, "path": "src/a.rs", "line": 4, "comments": {"nodes": [{"author": {"login": "c"}, "body": "¿y esto?"}]}},
                {"isResolved": true, "path": "x", "comments": {"nodes": []}}
            ]},
            "files": {"nodes": [{"path": "src/a.rs", "additions": 3, "deletions": 1, "changeType": "MODIFIED"}]},
            "closingIssuesReferences": {"nodes": [{"number": 3, "title": "P0.10", "state": "OPEN"}]},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
                {"__typename": "CheckRun", "name": "build", "conclusion": "FAILURE", "status": "COMPLETED"},
                {"__typename": "CheckRun", "name": "test", "conclusion": "SUCCESS", "status": "COMPLETED"}
            ]}}}}]}
        });
        let md = pr_card(&pr);
        assert!(md.starts_with("# #6 · t\n\n**abierto** · `feat/x` → `dev` · @a · +3 −1\n\n"));
        assert!(md.contains("- **Checks**: ✓ 1 · ✗ 1 · … 0 (falla: build)\n"));
        assert!(md.contains("- **Revisión**: aprobado · 1 hilos sin resolver\n"));
        assert!(md.contains("- **Cierra**: #3 P0.10 (abierto)\n"));
        assert!(md.contains(
            "> Descripción · Conversación (2) · Hilos sin resolver (1) · Archivos (1)\n"
        ));
        assert!(md.contains("_sin descripción_"));
        assert!(md.contains("### @b · 2026-10-16\n\nok"));
        assert!(md.contains("### @c · 2026-10-17 · aprobó"));
        assert!(!md.contains("ruido"));
        assert!(md.contains("### `src/a.rs:4`\n\n**@c**: ¿y esto?"));
        assert!(md.contains("- M `src/a.rs` +3 −1\n"));
    }

    #[test]
    fn issue_card_lists_tasks() {
        let md = issue_card(&json!({
            "number": 7, "title": "P0", "state": "OPEN", "url": "u", "author": {"login": "a"},
            "body": "cuerpo", "milestone": {"title": "P0 · Linux"}, "labels": {"nodes": [{"name": "solicitud"}]},
            "comments": {"nodes": []}, "parent": null,
            "subIssuesSummary": {"total": 2, "completed": 1},
            "subIssues": {"nodes": [{"number": 4, "title": "P0.1", "state": "CLOSED"}, {"number": 8, "title": "P0.2", "state": "OPEN"}]}
        }));
        assert!(md.contains("- **Milestone**: P0 · Linux\n- **Etiquetas**: solicitud\n"));
        assert!(md.contains("> Descripción · Tareas (1/2) · Conversación (0)\n"));
        assert!(md.contains("- [x] #4 P0.1\n- [ ] #8 P0.2\n"));
        assert!(!md.contains("Parte de"));
    }
}
