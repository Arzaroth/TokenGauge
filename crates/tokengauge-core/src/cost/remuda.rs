//! What remuda knows about who spent what: its proxy's request log and its
//! attribution ledger (`selvedge::usage`), read beside the transcripts.
//!
//! A request Claude Code sent through the proxy is in both its transcript and
//! the log, under the same ids, so the log adds no tokens for it, only the
//! login that spent it. A request no transcript holds - an SDK client's - is
//! an event of its own. Whatever the log does not name, the ledger does: the
//! login a `remuda run` session ran on, else the one the CLI's files held when
//! the request was billed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use selvedge::usage::account_digest;
use selvedge::usage::ledger::{self, Ledger};
use selvedge::usage::requests;

use super::{Source, UsageEvent, dedup_key};
use crate::providers::PROVIDERS;

/// Folds the proxy's requests into `events`. `seen` is the Claude reader's,
/// keyed the same way, so a request its transcript holds is found in it.
pub fn merge_requests(
    store: &Path,
    since: NaiveDate,
    events: &mut Vec<UsageEvent>,
    seen: &HashMap<u64, usize>,
    gaps: &mut Vec<PathBuf>,
) {
    let Ok(logged) = requests::read(store) else {
        gaps.push(store.join(requests::FILE));
        return;
    };
    let mut added: HashMap<u64, usize> = HashMap::new();
    for request in logged {
        let login = request.account_id.as_deref().map(account_digest);
        let key = request.dedup_parts().map(|(m, r)| dedup_key(m, r));
        if let Some(&index) = key.and_then(|k| seen.get(&k))
            && let Some(event) = events.get_mut(index)
        {
            if login.is_some() {
                event.login = login;
            }
            continue;
        }
        if request.tokens.total() == 0 {
            continue;
        }
        let Some(provider) = PROVIDERS.iter().copied().find(|p| *p == request.provider) else {
            continue;
        };
        let Some(at) = DateTime::<Utc>::from_timestamp_millis(request.at) else {
            continue;
        };
        let date = at.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }
        let event = UsageEvent {
            provider,
            model: request.model,
            date,
            at,
            tokens: request.tokens,
            key,
            one_request: true,
            service_tier: Default::default(),
            session: None,
            starts_session: false,
            login,
            source: Source::Proxy,
        };
        match key.and_then(|k| added.get(&k)) {
            Some(&index) => events[index] = event,
            None => {
                if let Some(k) = key {
                    added.insert(k, events.len());
                }
                events.push(event);
            }
        }
    }
}

/// Gives each event nothing named yet the login the ledger says spent it.
pub fn attribute(store: &Path, events: &mut [UsageEvent], gaps: &mut Vec<PathBuf>) {
    let ledger = match Ledger::load(store) {
        Ok(ledger) => ledger,
        Err(_) => {
            gaps.push(store.join(ledger::FILE));
            return;
        }
    };
    for event in events.iter_mut().filter(|e| e.login.is_none()) {
        event.login = ledger
            .login_at(
                event.provider,
                event.session.as_deref(),
                event.at.timestamp_millis(),
            )
            .map(account_digest);
    }
}

#[cfg(test)]
mod tests {
    use selvedge::usage::TokenCounts;
    use selvedge::usage::ledger::Entry;
    use selvedge::usage::requests::{Outcome, Request};

    use super::*;
    use crate::cost::{Roots, read_checked};

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tg-remuda-usage-{tag}-{}-{}",
                std::process::id(),
                crate::now_ms()
            ));
            std::fs::create_dir_all(dir.join("claude/project")).unwrap();
            std::fs::create_dir_all(dir.join("store")).unwrap();
            Fixture { dir }
        }

        fn store(&self) -> PathBuf {
            self.dir.join("store")
        }

        fn transcript(&self, lines: &[String]) {
            std::fs::write(
                self.dir.join("claude/project/session.jsonl"),
                lines.join("\n"),
            )
            .unwrap();
        }

        fn read(&self) -> Vec<UsageEvent> {
            let roots = Roots {
                claude: vec![self.dir.join("claude")],
                remuda: Some(self.store()),
                ..Roots::default()
            };
            read_checked(&roots, day("2026-10-01")).events
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn day(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn ms(text: &str) -> i64 {
        text.parse::<DateTime<Utc>>().unwrap().timestamp_millis()
    }

    fn line(at: &str, session: &str, message: &str, request: &str) -> String {
        serde_json::json!({
            "timestamp": at,
            "sessionId": session,
            "requestId": request,
            "message": {"id": message, "model": "claude-opus-4-5", "usage": {"input_tokens": 10, "output_tokens": 5}},
        })
        .to_string()
    }

    fn request(at: &str, message: &str, account: &str) -> Request {
        Request {
            at: ms(at),
            provider: "claude".into(),
            model: "claude-opus-4-5".into(),
            account_id: Some(account.into()),
            name: None,
            key: Some("k".into()),
            format: "messages".into(),
            stream: true,
            outcome: Outcome::Ok,
            status: 200,
            first_byte_ms: None,
            latency_ms: 10,
            tokens: TokenCounts {
                input: 100,
                output: 50,
                ..TokenCounts::default()
            },
            reasoning: 0,
            message_id: Some(message.into()),
            request_id: Some("req_p".into()),
            speed: None,
        }
    }

    fn live(at: &str, account: Option<&str>) -> Entry {
        Entry::Live {
            at: ms(at),
            provider: "claude".into(),
            account_id: account.map(str::to_owned),
            name: None,
        }
    }

    #[test]
    fn a_request_the_transcript_holds_counts_once_and_takes_the_proxy_s_login() {
        let f = Fixture::new("dedup");
        f.transcript(&[line("2026-10-20T10:00:00Z", "s1", "msg_1", "req_p")]);
        ledger::append(&f.store(), &live("2026-10-01T00:00:00Z", Some("u-live"))).unwrap();
        requests::append(
            &f.store(),
            &request("2026-10-20T10:00:00Z", "msg_1", "u-proxy"),
        )
        .unwrap();

        let events = f.read();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.input, 10, "the transcript's count stands");
        assert_eq!(events[0].source, Source::Cli);
        assert_eq!(events[0].login, Some(account_digest("u-proxy")));
    }

    #[test]
    fn a_request_no_transcript_holds_is_an_event_of_its_own() {
        let f = Fixture::new("proxy-only");
        f.transcript(&[]);
        requests::append(
            &f.store(),
            &request("2026-10-20T10:00:00Z", "chatcmpl-1", "u-proxy"),
        )
        .unwrap();
        let mut empty = request("2026-10-20T11:00:00Z", "chatcmpl-2", "u-proxy");
        empty.tokens = TokenCounts::default();
        requests::append(&f.store(), &empty).unwrap();
        requests::append(
            &f.store(),
            &request("2026-09-01T10:00:00Z", "chatcmpl-3", "u-proxy"),
        )
        .unwrap();

        let events = f.read();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, Source::Proxy);
        assert_eq!(events[0].provider, "claude");
        assert_eq!(events[0].tokens.output, 50);
        assert_eq!(events[0].login, Some(account_digest("u-proxy")));
    }

    #[test]
    fn the_ledger_names_the_rest_and_a_run_session_beats_the_live_login() {
        let f = Fixture::new("ledger");
        f.transcript(&[
            line("2026-10-05T10:00:00Z", "s-old", "m0", "r0"),
            line("2026-10-20T10:00:00Z", "s-live", "m1", "r1"),
            line("2026-10-20T10:05:00Z", "s-run", "m2", "r2"),
        ]);
        ledger::append(&f.store(), &live("2026-10-10T00:00:00Z", Some("u-live"))).unwrap();
        ledger::append(
            &f.store(),
            &Entry::Run {
                at: ms("2026-10-20T10:04:00Z"),
                provider: "claude".into(),
                session: "s-run".into(),
                account_id: "u-run".into(),
                name: None,
            },
        )
        .unwrap();

        let events = f.read();
        let login = |session: &str| {
            events
                .iter()
                .find(|e| e.session.as_deref() == Some(session))
                .unwrap()
                .login
                .clone()
        };

        assert_eq!(login("s-old"), None, "before the ledger, nobody's");
        assert_eq!(login("s-live"), Some(account_digest("u-live")));
        assert_eq!(login("s-run"), Some(account_digest("u-run")));
    }

    #[test]
    fn a_streamed_or_synthetic_first_record_still_starts_its_session() {
        let f = Fixture::new("streamed");
        let mut partial: serde_json::Value =
            serde_json::from_str(&line("2026-10-20T10:00:00Z", "s-stream", "m1", "r1")).unwrap();
        partial["message"]["usage"]["output_tokens"] = serde_json::json!(1);
        let mut synthetic: serde_json::Value =
            serde_json::from_str(&line("2026-10-20T08:00:00Z", "s-synth", "m9", "r9")).unwrap();
        synthetic["message"]["model"] = serde_json::json!("<synthetic>");
        f.transcript(&[
            partial.to_string(),
            line("2026-10-20T10:00:05Z", "s-stream", "m1", "r1"),
            synthetic.to_string(),
            line("2026-10-20T09:00:00Z", "s-synth", "m2", "r2"),
        ]);

        let events = f.read();
        let starts: Vec<(&str, bool)> = events
            .iter()
            .map(|e| (e.session.as_deref().unwrap(), e.starts_session))
            .collect();

        assert_eq!(starts, [("s-stream", true), ("s-synth", true)]);
    }

    #[test]
    fn a_session_counts_once_and_only_where_it_began() {
        let f = Fixture::new("sessions");
        f.transcript(&[
            line("2026-09-20T10:00:00Z", "s-before", "m0", "r0"),
            line("2026-10-20T10:00:00Z", "s-before", "m1", "r1"),
            line("2026-10-20T11:00:00Z", "s-new", "m2", "r2"),
            line("2026-10-21T11:00:00Z", "s-new", "m3", "r3"),
        ]);

        let events = f.read();
        let starts: Vec<(&str, bool)> = events
            .iter()
            .map(|e| (e.session.as_deref().unwrap(), e.starts_session))
            .collect();

        assert_eq!(
            starts,
            [("s-before", false), ("s-new", true), ("s-new", false)]
        );
    }
}
