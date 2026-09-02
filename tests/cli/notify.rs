//! Notifications against a wiremock GitHub API and Slack webhook: rolling-issue lifecycle
//! (create → comment → close), Slack block payloads, secret hygiene, `notify --last-run`,
//! and the `on_change` / `daily_digest` modes.

use super::support::*;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "ghp_SECRETTOKEN0123456789abcdefABCDEF00";
const REPO: &str = "acme/flywheel";

struct Api {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

impl Api {
    fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let server = rt.block_on(MockServer::start());
        Self { rt, server }
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(self.server.register(mock));
    }

    fn requests(&self) -> Vec<wiremock::Request> {
        self.rt.block_on(self.server.received_requests()).unwrap_or_default()
    }

    fn count(&self, m: &str, p: &str) -> usize {
        self.requests().iter().filter(|r| r.method.as_str() == m && r.url.path() == p).count()
    }
}

fn configure(fx: &mut Fixture, api: &Api, mode: &str, notify_on_success: bool) {
    fx.add_config_toml(&format!(
        "[notifications]\nenabled = true\nmode = \"{mode}\"\ngithub_token_env = \"AFSC_TEST_GH_TOKEN\"\ngithub_issue_repo = \"{REPO}\"\ngithub_api_url = \"{}\"\nslack_webhook_env = \"AFSC_TEST_SLACK\"\nslack_channel = \"#ops\"\nnotify_on_success = {notify_on_success}\n",
        api.uri()
    ));
}

fn env<'a>(api: &Api) -> Vec<(&'a str, String)> {
    vec![
        ("AFSC_ALLOW_LOCAL", "1".to_string()),
        ("AFSC_TEST_GH_TOKEN", TOKEN.to_string()),
        ("AFSC_TEST_SLACK", format!("{}/slack/hook/T000/B000/secretpath", api.uri())),
    ]
}

fn run_check(fx: &Fixture, api: &Api, extra: &[&str]) -> std::process::Output {
    let e = env(api);
    let set: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut args = vec!["check", "--local", "--format", "jsonl"];
    args.extend_from_slice(extra);
    fx.run_with(&args, &set, &[])
}

fn issues_path() -> String {
    format!("/repos/{REPO}/issues")
}

/// GitHub mock: the first issue listing is empty (nothing open yet); every later listing
/// returns the issue created by the first POST (#7). Comment, close and Slack always succeed.
fn mount_github(api: &Api) {
    let auth = || header("authorization", format!("Bearer {TOKEN}").as_str());
    api.mount(
        Mock::given(method("GET"))
            .and(path(issues_path()))
            .and(query_param("state", "open"))
            .and(auth())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .up_to_n_times(1),
    );
    api.mount(
        Mock::given(method("GET"))
            .and(path(issues_path()))
            .and(query_param("state", "open"))
            .and(auth())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "number": 7, "title": "AFSC canary: installer failures", "state": "open" }
            ]))),
    );
    api.mount(
        Mock::given(method("POST"))
            .and(path(issues_path()))
            .and(body_partial_json(serde_json::json!({ "labels": ["afsc-automated"] })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "number": 7, "html_url": "https://example.test/7" }))),
    );
    api.mount(
        Mock::given(method("POST"))
            .and(path(format!("{}/7/comments", issues_path())))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 }))),
    );
    api.mount(
        Mock::given(method("PATCH"))
            .and(path(format!("{}/7", issues_path())))
            .and(body_partial_json(serde_json::json!({ "state": "closed" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "number": 7, "state": "closed" }))),
    );
    api.mount(
        Mock::given(method("POST"))
            .and(path("/slack/hook/T000/B000/secretpath"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok")),
    );
}

#[test]
fn github_issue_is_created_then_commented_then_closed_and_slack_gets_blocks() {
    let api = Api::start();
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    configure(&mut fx, &api, "every_run", false);
    fx.add_pass("good_tool");
    fx.add_flaky("wobbly_tool", 2); // fails twice, then passes

    // Run 1: no open issue → create; Slack gets a failure payload.
    mount_github(&api);
    let out = run_check(&fx, &api, &["-vvv"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(api.count("POST", &issues_path()), 1, "issue created once");
    assert_eq!(api.count("POST", &format!("{}/7/comments", issues_path())), 0);
    let create = api.requests().into_iter().find(|r| r.method == "POST" && r.url.path() == issues_path()).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(body["title"], "AFSC canary: installer failures");
    let md = body["body"].as_str().unwrap();
    assert!(md.contains("| wobbly_tool | ❌ failed | network |"), "{md}");
    assert!(md.contains("Results file:"), "{md}");
    let slack = api.requests().into_iter().find(|r| r.url.path().starts_with("/slack/")).expect("slack posted");
    let payload: serde_json::Value = serde_json::from_slice(&slack.body).unwrap();
    assert_eq!(payload["channel"], "#ops");
    assert_eq!(payload["blocks"][0]["type"], "header");
    let text = payload.to_string();
    assert!(text.contains("*wobbly_tool* — failed (network"), "{text}");
    assert!(text.contains("Connection refused"), "failure hint included: {text}");
    // Secret hygiene: verbose logs never contain the token or the webhook path.
    let logs = stderr(&out);
    assert!(!logs.contains(TOKEN), "token leaked to stderr:\n{logs}");
    assert!(!logs.contains("secretpath"), "webhook URL leaked to stderr:\n{logs}");
    assert!(logs.contains("Created GitHub issue"), "{logs}");

    // Run 2: still failing, issue open → comment, no new issue.
    let out = run_check(&fx, &api, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(api.count("POST", &issues_path()), 1, "no duplicate issue");
    assert_eq!(api.count("POST", &format!("{}/7/comments", issues_path())), 1);
    assert_eq!(api.count("PATCH", &format!("{}/7", issues_path())), 0);
    assert_eq!(api.requests().iter().filter(|r| r.url.path().starts_with("/slack/")).count(), 2);

    // Run 3: clean → recovery comment + close; Slack silent (notify_on_success = false).
    let out = run_check(&fx, &api, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(api.count("POST", &format!("{}/7/comments", issues_path())), 2);
    assert_eq!(api.count("PATCH", &format!("{}/7", issues_path())), 1);
    assert_eq!(api.requests().iter().filter(|r| r.url.path().starts_with("/slack/")).count(), 2, "no Slack on success");
    let comment = api.requests().into_iter().filter(|r| r.url.path().ends_with("/comments")).last().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&comment.body).unwrap();
    assert!(body["body"].as_str().unwrap().starts_with("Recovered:"), "{body}");

    // The event log records the notification outcome.
    let log_dir = fx.home.join(".local/share/afsc/logs");
    let log = std::fs::read_dir(&log_dir).unwrap().flatten().next().unwrap().path();
    let text = std::fs::read_to_string(log).unwrap();
    let events: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let notes: Vec<&serde_json::Value> = events.iter().filter(|e| e["event"] == "notification").collect();
    assert_eq!(notes.len(), 3, "{text}");
    assert_eq!(notes[0]["data"]["github"], "created");
    assert_eq!(notes[1]["data"]["github"], "commented");
    assert_eq!(notes[2]["data"]["github"], "closed");
    assert_eq!(notes[2]["data"]["kind"], "recovered");
}

#[test]
fn notify_last_run_resends_and_requires_notifications_enabled() {
    let api = Api::start();
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_dependency_failure("dep_fail_tool");
    // Disabled: the check sends nothing and notify refuses.
    let out = run_check(&fx, &api, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(api.requests().is_empty());
    let refused = fx.run(&["notify", "--last-run"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(stderr(&refused).contains("notifications are disabled"));

    configure(&mut fx, &api, "on_change", false);
    mount_github(&api);
    let e = env(&api);
    let set: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = fx.run_with(&["notify", "--last-run", "--format", "json"], &set, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "notify");
    assert_eq!(doc["decision"], "sent");
    assert_eq!(doc["github"], "created");
    assert_eq!(doc["github_issue"], 7);
    assert_eq!(doc["slack"], "sent");
    assert_eq!(api.count("POST", &issues_path()), 1);
    let missing = fx.run_with(&["notify", "--run", "zzzzzzzz"], &set, &[]);
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn on_change_mode_sends_only_on_transitions() {
    let api = Api::start();
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    configure(&mut fx, &api, "on_change", false);
    fx.add_flaky("wobbly_tool", 2);
    mount_github(&api);

    // Run 1 fails → sent (created). Run 2 fails identically → unchanged, nothing sent.
    let out = run_check(&fx, &api, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(api.count("POST", &issues_path()), 1);
    let before = api.requests().len();
    let out = run_check(&fx, &api, &["-v"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(api.requests().len(), before, "identical failure set: no traffic at all");
    assert!(stderr(&out).contains("unchanged"), "{}", stderr(&out));

    // Run 3 recovers → sent as a recovery (issue closed).
    let out = run_check(&fx, &api, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(api.count("PATCH", &format!("{}/7", issues_path())), 1);

    // Run 4 still clean → unchanged again.
    let before = api.requests().len();
    let out = run_check(&fx, &api, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(api.requests().len(), before, "clean after clean: silent");
}

#[test]
fn daily_digest_queues_runs_and_sends_once() {
    let api = Api::start();
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    configure(&mut fx, &api, "daily_digest", false);
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    mount_github(&api);

    run_check(&fx, &api, &[]);
    run_check(&fx, &api, &[]);
    assert!(api.requests().is_empty(), "digest mode queues instead of sending");
    let pending = fx.home.join(".local/share/afsc/notify/pending.jsonl");
    assert_eq!(std::fs::read_to_string(&pending).unwrap().lines().count(), 2);

    let e = env(&api);
    let set: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = fx.run_with(&["notify", "--digest", "--format", "json"], &set, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(doc["decision"], "sent");
    assert_eq!(doc["runs"], 2);
    assert_eq!(doc["github"], "created");
    let create = api.requests().into_iter().find(|r| r.method == "POST" && r.url.path() == issues_path()).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&create.body).unwrap();
    let md = body["body"].as_str().unwrap();
    assert!(md.contains("## AFSC daily digest — 2 run(s)"), "{md}");
    assert!(md.contains("dep_fail_tool"), "{md}");
    assert!(!pending.exists(), "queue rotated after sending");
    assert!(std::fs::read_dir(pending.parent().unwrap()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("sent_")));

    let again = fx.run_with(&["notify", "--digest", "--format", "json"], &set, &[]);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(json_doc(&again)["decision"], "nothing_pending");
}
