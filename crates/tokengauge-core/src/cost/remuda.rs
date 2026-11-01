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
