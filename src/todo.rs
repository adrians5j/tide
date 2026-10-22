//! A scratch todo list, shared by every workspace.
//!
//! The point is capture: a thought arrives while you're reading code, and it
//! should cost one shortcut and one line to keep it. So the list is global (not
//! per repo — a todo is not a property of the checkout you happened to be in
//! when it occurred to you), and nothing here needs a window, which is why the
//! whole model lives in this file and is tested without one.

use std::path::PathBuf;

/// Where a line stands. Three states, because "started" is the thing a plain
/// checkbox can't say and the thing you most want to see on a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Todo,
    Doing,
    Done,
}

impl Status {
    pub fn key(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Doing => "doing",
            Status::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Todo => "Todo",
            Status::Doing => "In progress",
            Status::Done => "Done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" | "open" => Some(Status::Todo),
            "doing" | "in_progress" | "wip" => Some(Status::Doing),
            "done" | "closed" => Some(Status::Done),
            _ => None,
        }
    }

    /// The three, in the order the form offers them.
    pub const ALL: [Status; 3] = [Status::Todo, Status::Doing, Status::Done];

    /// Clicking a row walks it forward: todo → doing → done → todo.
    pub fn next(self) -> Self {
        match self {
            Status::Todo => Status::Doing,
            Status::Doing => Status::Done,
            Status::Done => Status::Todo,
        }
    }
}

/// One line on the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub id: String,
    /// What it is, in one line. The form calls this the description.
    pub text: String,
    /// The rest of it: context, links, what was decided. Mostly what an agent
    /// fills in when it backlogs something out of a session, since the one-line
    /// description never holds the reasoning.
    pub details: String,
    /// A type key from TIDE's conventional-commit set (`feat`, `fix`, …), or
    /// empty. The list shows its icon; the form picks it.
    pub category: String,
    pub status: Status,
    /// Optional URL: a PR, an issue, a Slack thread.
    pub link: String,
    /// Optional free text: `cli`, `sdk`, `admin`.
    pub scope: String,
    /// Who it's for, when it's client work.
    pub client: String,
    /// What this line belongs with. A todo isn't owned by a workspace, but a
    /// batch an agent backlogs out of one session belongs together, and the
    /// list shows each group under its own heading.
    pub group: String,
    /// The workspace folder it was written in. Provenance, not ownership: the
    /// list never filters by it on its own, but "where was I when I wrote this"
    /// is the question you ask a week later, and the folder is the answer that
    /// survives a rename or a reset.
    pub origin: String,
    /// Unix seconds, for ordering and for "written 2 days ago".
    pub created: u64,
}

impl Todo {
    pub fn done(&self) -> bool {
        self.status == Status::Done
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/todos.json"))
}

/// When the file last changed, so a workspace can notice another one's edits
/// without re-reading the list on every poll.
pub fn mtime() -> Option<u64> {
    let m = std::fs::metadata(path()?).ok()?.modified().ok()?;
    m.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

pub fn load() -> Vec<Todo> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|body| from_json(&body))
        .unwrap_or_default()
}

pub fn save(list: &[Todo]) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, to_json(list));
}

pub fn to_json(list: &[Todo]) -> String {
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "text": t.text,
                "details": t.details,
                "category": t.category,
                "status": t.status.key(),
                "link": t.link,
                "scope": t.scope,
                "client": t.client,
                "group": t.group,
                "origin": t.origin,
                "created": t.created,
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into())
}

pub fn from_json(body: &str) -> Vec<Todo> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(body) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            let text = v.get("text")?.as_str()?.to_string();
            if text.trim().is_empty() {
                return None;
            }
            // `done: true` is how the first version of the list wrote it
            let status = v
                .get("status")
                .and_then(|s| s.as_str())
                .and_then(Status::parse)
                .unwrap_or_else(|| {
                    match v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                        true => Status::Done,
                        false => Status::Todo,
                    }
                });
            Some(Todo {
                id: v.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string(),
                text,
                details: v.get("details").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                category: v.get("category").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                status,
                link: v.get("link").and_then(|l| l.as_str()).unwrap_or("").to_string(),
                scope: v.get("scope").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                client: v.get("client").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                // the first version tagged each line with the workspace it was
                // written in; that reads as a group now
                group: v
                    .get("group")
                    .or_else(|| v.get("tag"))
                    .and_then(|g| g.as_str())
                    .unwrap_or("")
                    .to_string(),
                origin: v.get("origin").and_then(|o| o.as_str()).unwrap_or("").to_string(),
                created: v.get("created").and_then(|c| c.as_u64()).unwrap_or(0),
            })
        })
        .collect()
}

/// Add a line. `id` comes from the caller so this file needs no randomness.
/// Everything but the description is optional, which is what makes the form
/// worth filling in rather than a chore.
#[allow(clippy::too_many_arguments)]
pub fn add(
    list: &mut Vec<Todo>,
    id: String,
    text: &str,
    details: &str,
    category: &str,
    status: Status,
    link: &str,
    scope: &str,
    client: &str,
    group: &str,
    origin: &str,
) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    list.push(Todo {
        id,
        text: text.to_string(),
        details: details.trim().to_string(),
        category: category.to_string(),
        status,
        link: link.trim().to_string(),
        scope: scope.trim().to_string(),
        client: client.trim().to_string(),
        group: group.trim().to_string(),
        origin: origin.trim().to_string(),
        created: now(),
    });
    true
}

/// Walk a line's status forward (what clicking it does).
pub fn advance(list: &mut [Todo], id: &str) {
    if let Some(t) = list.iter_mut().find(|t| t.id == id) {
        t.status = t.status.next();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn set_status(list: &mut [Todo], id: &str, status: Status) {
    if let Some(t) = list.iter_mut().find(|t| t.id == id) {
        t.status = status;
    }
}

pub fn remove(list: &mut Vec<Todo>, id: &str) {
    list.retain(|t| t.id != id);
}

/// Drop everything already ticked off.
pub fn clear_done(list: &mut Vec<Todo>) -> usize {
    let before = list.len();
    list.retain(|t| !t.done());
    before - list.len()
}

pub fn open_count(list: &[Todo]) -> usize {
    list.iter().filter(|t| !t.done()).count()
}

/// The list as it's shown: what's still open first, newest at the top of each
/// group, and anything matching `query` (its text, scope, client, category,
/// group or the workspace it was written in). Returns
/// indices into `list`, so a row can act on the real item.
pub fn view(list: &[Todo], query: &str, origin: Option<&str>) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let mut ix: Vec<usize> = (0..list.len())
        // `origin` narrows the list to one workspace. A line with no origin at
        // all (written before TIDE recorded it) belongs everywhere rather than
        // nowhere — hiding it would look like losing it.
        .filter(|&i| {
            origin.is_none_or(|o| list[i].origin.is_empty() || list[i].origin == o)
        })
        .filter(|&i| {
            q.is_empty()
                || list[i].text.to_lowercase().contains(&q)
                || list[i].details.to_lowercase().contains(&q)
                || list[i].scope.to_lowercase().contains(&q)
                || list[i].client.to_lowercase().contains(&q)
                || list[i].group.to_lowercase().contains(&q)
                || list[i].origin.to_lowercase().contains(&q)
                || list[i].category.to_lowercase().contains(&q)
        })
        .collect();
    // in progress first, then todo, then done; newest at the top of each group
    let rank = |t: &Todo| match t.status {
        Status::Doing => 0,
        Status::Todo => 1,
        Status::Done => 2,
    };
    ix.sort_by(|&a, &b| {
        (rank(&list[a]), std::cmp::Reverse(list[a].created))
            .cmp(&(rank(&list[b]), std::cmp::Reverse(list[b].created)))
    });
    ix
}

/// The view, split into its groups: `(group name, rows)`, groups ordered by
/// their newest line, and whatever has no group last under an empty name. The
/// rows inside each group keep [`view`]'s order.
pub fn grouped(list: &[Todo], query: &str, origin: Option<&str>) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for i in view(list, query, origin) {
        let g = list[i].group.clone();
        match out.iter_mut().find(|(name, _)| *name == g) {
            Some((_, rows)) => rows.push(i),
            None => out.push((g, vec![i])),
        }
    }
    let newest = |rows: &[usize]| rows.iter().map(|&i| list[i].created).max().unwrap_or(0);
    out.sort_by_key(|(name, rows)| (name.is_empty(), std::cmp::Reverse(newest(rows))));
    out
}

/// "just now", "3h", "2d" — enough to tell a fresh note from a stale one.
pub fn age(created: u64) -> String {
    if created == 0 {
        return String::new();
    }
    let secs = now().saturating_sub(created);
    match secs {
        0..=90 => "just now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(id: &str, text: &str, status: Status, created: u64, scope: &str) -> Todo {
        Todo {
            id: id.into(),
            text: text.into(),
            details: String::new(),
            category: String::new(),
            status,
            link: String::new(),
            scope: scope.into(),
            client: String::new(),
            group: String::new(),
            origin: String::new(),
            created,
        }
    }

    fn sample() -> Vec<Todo> {
        vec![
            todo("a", "ask mate about the proxy", Status::Todo, 100, "cli"),
            todo("b", "write the changelog", Status::Done, 200, ""),
            todo("c", "chase the flaky test", Status::Todo, 300, "sdk"),
        ]
    }

    /// Open lines first, newest at the top of each group: what you just wrote is
    /// where you look, and what's done sinks.
    #[test]
    fn the_view_puts_open_first_and_newest_on_top() {
        let list = sample();
        let ids: Vec<&str> = view(&list, "", None).iter().map(|&i| list[i].id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b"]);
    }

    /// Filtering matches the way you'd search for a line: its words, or the
    /// scope you filed it under.
    #[test]
    fn the_filter_matches_text_or_scope() {
        let list = sample();
        let by_text: Vec<&str> = view(&list, "proxy", None).iter().map(|&i| list[i].id.as_str()).collect();
        assert_eq!(by_text, ["a"]);
        // what an agent wrote into the details is searchable too
        let mut with_details = list.clone();
        with_details[1].details = "the 6.4 backport needs it".into();
        let by_details: Vec<&str> =
            view(&with_details, "backport", None).iter().map(|&i| with_details[i].id.as_str()).collect();
        assert_eq!(by_details, ["b"]);
        let by_scope: Vec<&str> = view(&list, "sdk", None).iter().map(|&i| list[i].id.as_str()).collect();
        assert_eq!(by_scope, ["c"]);
        let mut with_client = list.clone();
        with_client[1].client = "Acme".into();
        let by_client: Vec<&str> =
            view(&with_client, "acme", None).iter().map(|&i| with_client[i].id.as_str()).collect();
        assert_eq!(by_client, ["b"]);
        // and the workspace it came out of, which nobody types but everybody
        // wants back later
        let mut with_origin = list.clone();
        with_origin[2].origin = "wby-next4".into();
        let by_origin: Vec<&str> =
            view(&with_origin, "next4", None).iter().map(|&i| with_origin[i].id.as_str()).collect();
        assert_eq!(by_origin, ["c"]);
        assert!(view(&list, "nothing here", None).is_empty());
    }

    #[test]
    fn adding_advancing_and_clearing() {
        let mut list = sample();
        assert!(add(
            &mut list,
            "d".into(),
            "  fix the header  ",
            "  it clips at 320px  ",
            "fix",
            Status::Todo,
            " https://x/1 ",
            " admin ",
            " Acme ",
            " license flags ",
            "wby-next4"
        ));
        let added = list.last().unwrap();
        assert_eq!(added.text, "fix the header", "trimmed");
        assert_eq!(added.details, "it clips at 320px");
        assert_eq!(added.link, "https://x/1");
        assert_eq!(added.scope, "admin");
        assert_eq!(added.category, "fix");
        assert_eq!(added.client, "Acme");
        assert_eq!(added.group, "license flags");
        assert_eq!(added.origin, "wby-next4", "where it was written, recorded for you");
        assert!(
            !add(&mut list, "e".into(), "  ", "", "", Status::Todo, "", "", "", "", ""),
            "blank isn't a task"
        );
        assert_eq!(open_count(&list), 3);
        // a click walks it forward rather than flipping a checkbox
        advance(&mut list, "d");
        assert_eq!(list.last().unwrap().status, Status::Doing);
        assert_eq!(open_count(&list), 3, "in progress is still open");
        advance(&mut list, "d");
        assert_eq!(list.last().unwrap().status, Status::Done);
        assert_eq!(open_count(&list), 2);
        advance(&mut list, "d");
        assert_eq!(list.last().unwrap().status, Status::Todo, "and around again");
        set_status(&mut list, "d", Status::Done);
        remove(&mut list, "a");
        assert_eq!(clear_done(&mut list), 2, "the ticked ones go");
        assert_eq!(open_count(&list), 1);
    }

    /// In progress sits above what hasn't been started, and done sinks.
    #[test]
    fn the_view_leads_with_what_is_under_way() {
        let mut list = sample();
        set_status(&mut list, "a", Status::Doing);
        let ids: Vec<&str> = view(&list, "", None).iter().map(|&i| list[i].id.as_str()).collect();
        assert_eq!(ids, ["a", "c", "b"]);
    }

    /// The first version of the file wrote `done: true` and nothing else.
    #[test]
    fn an_old_file_still_loads() {
        let old = r#"[{"id":"x","text":"old line","done":true,"created":5}]"#;
        let list = from_json(old);
        assert_eq!(list[0].status, Status::Done);
        assert!(list[0].category.is_empty() && list[0].link.is_empty());
    }

    /// A batch an agent backlogs out of one session reads as one block, and
    /// loose lines sit at the end rather than inventing a group for themselves.
    #[test]
    fn grouping_keeps_a_batch_together() {
        let mut list = sample();
        list[0].group = "license flags".into();
        list[1].group = "license flags".into();
        let groups = grouped(&list, "", None);
        let names: Vec<&str> = groups.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["license flags", ""], "the loose line is last");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 1);
        // filtering by the group name keeps that block and drops the rest
        assert_eq!(grouped(&list, "license", None).len(), 1);
    }

    /// "this project only": lines from elsewhere drop out, and lines from
    /// before TIDE recorded a workspace stay — they belong to no project, which
    /// is not the same as belonging to another one.
    #[test]
    fn narrowing_to_one_workspace() {
        let mut list = sample();
        list[0].origin = "wby-next4".into();
        list[1].origin = "wby-next12".into();
        let here: Vec<&str> =
            view(&list, "", Some("wby-next4")).iter().map(|&i| list[i].id.as_str()).collect();
        assert_eq!(here, ["c", "a"], "this workspace's, plus the one with no origin");
        let there: Vec<&str> =
            view(&list, "", Some("wby-next12")).iter().map(|&i| list[i].id.as_str()).collect();
        assert_eq!(there, ["c", "b"]);
    }

    /// The first version wrote the workspace as `tag`; that reads as a group.
    #[test]
    fn an_old_tag_becomes_a_group() {
        let old = r#"[{"id":"x","text":"old line","created":5,"tag":"wby-next6"}]"#;
        assert_eq!(from_json(old)[0].group, "wby-next6");
    }

    #[test]
    fn round_trips_through_json() {
        let list = sample();
        assert_eq!(from_json(&to_json(&list)), list);
        // a file from a future version, or a hand-edited one, degrades quietly
        assert!(from_json("not json").is_empty());
        assert_eq!(from_json(r#"[{"text":"bare"}]"#).len(), 1);
        assert!(from_json(r#"[{"text":"   "}]"#).is_empty());
    }

    #[test]
    fn age_reads_at_a_glance() {
        let t = now();
        assert_eq!(age(t), "just now");
        assert_eq!(age(t - 600), "10m");
        assert_eq!(age(t - 7200), "2h");
        assert_eq!(age(t - 3 * 86_400), "3d");
        assert!(age(0).is_empty(), "no timestamp, nothing to say");
    }
}
