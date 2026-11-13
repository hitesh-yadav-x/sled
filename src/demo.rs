//! Demo mode (`sled serve --demo`): every word that could identify a project or what a
//! session was about is replaced with made-up text before it leaves the server. Times,
//! token counts, costs, models and session ids stay real.

use crate::db::SessionRow;
use crate::server::resume_command;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const PROJECTS: &[&str] = &[
    "atlas-api",
    "storefront-web",
    "billing-service",
    "mobile-app",
    "data-pipeline",
    "design-system",
    "search-indexer",
    "infra-terraform",
    "docs-site",
    "auth-gateway",
    "ml-notebooks",
    "admin-console",
];

const BRANCHES: &[&str] = &[
    "main",
    "feat/rate-limits",
    "fix/flaky-tests",
    "main",
    "feat/export-csv",
    "chore/upgrade-deps",
    "feat/onboarding",
    "fix/timezone-bug",
];

/// One made-up session: title, recap, first prompt, last prompt.
const STORIES: &[[&str; 4]] = &[
    [
        "Add rate limiting to the public API",
        "Added a token-bucket limiter in front of the public routes, with per-key limits read from config. Tests cover bursts and refill. Next: wire the limit headers into the response.",
        "We keep getting hammered by one client. Can you add rate limiting to the public API?",
        "Looks good. Now add the remaining-quota headers to each response.",
    ],
    [
        "Fix flaky checkout integration test",
        "Traced the flaky checkout test to a shared fixture that two tests mutated. Gave each test its own cart and the suite passed 50 runs in a row.",
        "The checkout integration test fails about one run in ten. Find out why.",
        "Run it 50 times to be sure, then commit.",
    ],
    [
        "Migrate settings page to the new form components",
        "Moved the settings page onto the shared form components and removed the old validation helpers. Three fields still use the legacy date picker.",
        "Migrate the settings page to the new form components.",
        "Leave the date picker for now and open a follow-up issue.",
    ],
    [
        "Speed up the nightly export job",
        "The nightly export was reading one row at a time. Switched to batched reads and a streaming writer; the job went from 41 minutes to 6 on last night's data.",
        "The nightly export takes over 40 minutes. Where is the time going?",
        "Great. Add a metric for rows per second so we notice if it regresses.",
    ],
    [
        "Add CSV export to the reports screen",
        "Added a CSV export button to the reports screen. It streams the current filter's rows and quotes fields correctly. Waiting on a decision about the date format.",
        "Users want to download the reports table as CSV.",
        "Use ISO dates for now.",
    ],
    [
        "Investigate memory growth in the worker",
        "Found the worker holding every processed job in an in-memory list used only for a debug log. Removed it and memory stays flat over a two-hour soak.",
        "The worker's memory climbs until it gets killed. Can you find the leak?",
        "Run the soak test again with the fix.",
    ],
    [
        "Write onboarding guide for new contributors",
        "Drafted a contributor guide: setup, running tests, the branch and review flow, and where the main modules live. Needs a pass from someone new to the code.",
        "Write an onboarding guide for new contributors.",
        "Add a short section on how releases are cut.",
    ],
    [
        "Upgrade dependencies and fix breaking changes",
        "Upgraded the HTTP client and the test runner to their latest major versions and fixed the 14 call sites that broke. All tests pass.",
        "Upgrade our dependencies to current versions and fix whatever breaks.",
        "Check the changelog for the HTTP client once more for behaviour changes.",
    ],
    [
        "Fix timezone bug in the weekly summary",
        "The weekly summary grouped by UTC day, so late-evening events landed in the next day. It now groups in the account's timezone, with tests around midnight.",
        "Some events show up on the wrong day in the weekly summary.",
        "Add a test for an account in a timezone ahead of UTC too.",
    ],
    [
        "Add search to the admin user list",
        "Added server-side search to the admin user list, matching name and email, with a debounced input and paging kept in the URL.",
        "The admin user list needs a search box.",
        "Make the search case-insensitive.",
    ],
    [
        "Refactor the notification sender",
        "Split the notification sender into one module per channel behind a common interface. Email and push are done; SMS still goes through the old path.",
        "The notification code is one 900-line file. Can you break it up sensibly?",
        "Do SMS next, same pattern.",
    ],
    [
        "Set up CI caching for faster builds",
        "Cached the dependency and build folders in CI, keyed on the lockfile. A typical pull request build dropped from 11 minutes to 4.",
        "CI builds are slow. Can we cache more?",
        "Document how to clear the cache when it goes stale.",
    ],
    [
        "Add audit log for permission changes",
        "Permission changes now write an audit record with who, what and when, and the admin screen lists them newest first.",
        "We need an audit trail for permission changes.",
        "Add a filter by user on the audit screen.",
    ],
    [
        "Review error handling in the payment webhook",
        "Reviewed the payment webhook: two paths swallowed errors and returned 200. They now return 500 so the provider retries, and each failure is logged with the event id.",
        "Review the error handling in the payment webhook handler.",
        "Add a test that the provider's retry is handled without double-charging.",
    ],
    [
        "Prototype dark mode for the dashboard",
        "Built a dark theme for the dashboard from colour tokens, behind a setting. Charts still use the light palette.",
        "Prototype a dark mode for the dashboard.",
        "Now do the charts.",
    ],
    [
        "Document the deployment runbook",
        "Wrote the deployment runbook: pre-checks, the rollout steps, how to watch it, and how to roll back. Verified each command against staging.",
        "Write down how we deploy, step by step, so anyone on call can do it.",
        "Add what to do if the migration step fails halfway.",
    ],
];

/// Maps each real session and folder to its made-up stand-in. Built from every session in
/// the ledger, so the list and the detail view always agree.
pub struct Mask {
    /// Session id to its position among all sessions, oldest first.
    rank: BTreeMap<String, usize>,
    /// Real folder to its made-up project name.
    project: BTreeMap<String, String>,
}

impl Mask {
    pub fn new(all: &[SessionRow]) -> Mask {
        let mut order: Vec<&SessionRow> = all.iter().collect();
        order.sort_by(|a, b| (a.started_at, &a.session_id).cmp(&(b.started_at, &b.session_id)));
        let rank = order
            .iter()
            .enumerate()
            .map(|(i, r)| (r.session_id.clone(), i))
            .collect();
        let mut project = BTreeMap::new();
        for dir in all.iter().filter_map(|r| r.project_dir.as_deref()).filter(|d| !d.is_empty()) {
            let n = project.len();
            project.entry(dir.to_string()).or_insert_with(|| match PROJECTS.get(n) {
                Some(name) => name.to_string(),
                None => format!("project-{}", n + 1),
            });
        }
        Mask { rank, project }
    }

    fn story(&self, session_id: &str) -> &'static [&'static str; 4] {
        &STORIES[self.rank.get(session_id).copied().unwrap_or(0) % STORIES.len()]
    }

    pub fn title(&self, session_id: &str) -> &'static str {
        self.story(session_id)[0]
    }

    /// Rewrites one session object from the API (list or detail form) in place.
    pub fn apply(&self, v: &mut Value) {
        let id = v["id"].as_str().unwrap_or("").to_string();
        let real_dir = v["project_dir"].as_str().unwrap_or("").to_string();
        let story = self.story(&id);
        let rank = self.rank.get(&id).copied().unwrap_or(0);
        let name = self.project.get(&real_dir).cloned();
        let dir = name.as_ref().map(|n| format!("~/work/{n}"));

        let replace_if_set = |v: &mut Value, key: &str, with: &str| {
            if v.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
                v[key] = json!(with);
            }
        };

        v["title"] = json!(story[0]);
        match v["description_kind"].as_str() {
            Some("recap") => v["description"] = json!(story[1]),
            Some("first_prompt") => v["description"] = json!(story[2]),
            _ => v["description"] = json!(""),
        }
        v["project"] = json!(name.clone().unwrap_or_default());
        v["project_dir"] = json!(dir.clone().unwrap_or_default());
        replace_if_set(v, "branch", BRANCHES[rank % BRANCHES.len()]);
        v["note"] = json!("");
        replace_if_set(v, "first_prompt", story[2]);
        replace_if_set(v, "last_prompt", story[3]);
        if v.get("resume_command").is_some_and(|c| !c.is_null()) {
            v["resume_command"] = match &dir {
                Some(d) => json!(resume_command(d, &id)),
                None => Value::Null,
            };
            v["dir_exists"] = json!(true);
        }
        if let Some(other) = v.get("continued_from").and_then(|c| c["id"].as_str()).map(str::to_string) {
            v["continued_from"]["title"] = json!(self.title(&other));
        }
        v["demo"] = json!(true);
    }
}
