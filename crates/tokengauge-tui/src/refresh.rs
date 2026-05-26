use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use tokengauge_core::{
    FetchResult, ProviderFetchError, ProviderRow, fetch_all_providers, load_config,
    payload_to_rows_with_costs, read_cache_full, write_cache_full, write_default_config,
};

/// Outcome of a background fetch.
pub struct RefreshResult {
    pub rows: Vec<ProviderRow>,
    pub errors: Vec<ProviderFetchError>,
}

/// Kick off a fetch in a worker thread; the caller polls the returned receiver.
pub fn spawn_refresh(
    config_override: Option<PathBuf>,
    force: bool,
) -> Receiver<Result<RefreshResult>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(fetch_rows_with_config(config_override, force));
    });
    receiver
}

fn fetch_rows_with_config(
    config_override: Option<PathBuf>,
    force: bool,
) -> Result<RefreshResult> {
    let config_path = config_override.unwrap_or_else(tokengauge_core::default_config_path);
    if !config_path.exists() {
        write_default_config(&config_path)?;
    }
    let config = load_config(Some(config_path))?;
    let cached = read_cache_full(&config.cache_file).ok();

    let stale = fs::metadata(&config.cache_file)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|age| age >= Duration::from_secs(config.refresh_secs))
        .unwrap_or(true);

    let (payloads, errors, costs) = match cached {
        Some(c) if !force && !stale => c.into_parts(),
        _ => {
            let FetchResult {
                payloads,
                errors,
                costs,
            } = fetch_all_providers(&config);
            write_cache_full(&config.cache_file, &payloads, &errors, &costs).ok();
            (payloads, errors, costs)
        }
    };

    Ok(RefreshResult {
        rows: payload_to_rows_with_costs(payloads, &costs),
        errors,
    })
}
