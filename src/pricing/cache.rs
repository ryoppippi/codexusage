use super::{CacheDecision, PricingCache};
use eyre::{Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default time-to-live for the local pricing cache.
pub(super) const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// `LiteLLM` model pricing dataset used for refreshes.
const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
/// Connect timeout for pricing refreshes.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Total request timeout for pricing refreshes.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the on-disk cache path.
#[must_use]
pub fn default_cache_path() -> PathBuf {
    if let Some(base) = dirs::cache_dir() {
        return base.join("codexusage").join("pricing-cache.json");
    }

    PathBuf::from(".codexusage-pricing-cache.json")
}

/// Decide whether pricing should be refreshed.
///
/// # Errors
///
/// Returns an error only when the current time cannot be compared safely against the cache
/// payload. Invalid or unreadable cache files are treated as refresh candidates.
pub fn decide_cache_action(
    cache_path: &Path,
    now: SystemTime,
    ttl: Duration,
    offline: bool,
    force_refresh: bool,
) -> Result<CacheDecision> {
    if offline {
        return Ok(CacheDecision::UseCache);
    }
    if force_refresh {
        return Ok(CacheDecision::Refresh);
    }
    let Ok(cache) = read_cache(cache_path) else {
        return Ok(CacheDecision::Refresh);
    };
    let Some(age) = cache_age(&cache, now) else {
        return Ok(CacheDecision::Refresh);
    };
    if age <= ttl {
        Ok(CacheDecision::UseCache)
    } else {
        Ok(CacheDecision::Refresh)
    }
}

/// Compute the age of a cache payload from its embedded refresh timestamp.
pub(super) fn cache_age(cache: &PricingCache, now: SystemTime) -> Option<Duration> {
    let refreshed_at_epoch_seconds = u64::try_from(cache.refreshed_at_epoch_seconds).ok()?;
    let refreshed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(refreshed_at_epoch_seconds);
    Some(
        now.duration_since(refreshed_at)
            .unwrap_or_else(|_| Duration::from_secs(0)),
    )
}

/// Read the persisted pricing cache.
pub(super) fn read_cache(cache_path: &Path) -> Result<PricingCache> {
    let raw = fs::read_to_string(cache_path)
        .wrap_err_with(|| format!("failed to read pricing cache {}", cache_path.display()))?;
    serde_json::from_str(&raw).wrap_err("failed to parse pricing cache")
}

/// Write the persisted pricing cache atomically.
pub(super) fn write_cache(cache_path: &Path, cache: &PricingCache) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create cache directory {}", parent.display()))?;
    }
    let temporary = cache_path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(cache)?)
        .wrap_err_with(|| format!("failed to write temporary cache {}", temporary.display()))?;
    replace_cache_file(&temporary, cache_path)
}

/// Replace the destination cache file in a cross-platform way.
fn replace_cache_file(temporary: &Path, destination: &Path) -> Result<()> {
    if let Err(error) = fs::rename(temporary, destination) {
        if !destination.exists() {
            return Err(error).wrap_err_with(|| {
                format!("failed to replace pricing cache {}", destination.display())
            });
        }

        fs::remove_file(destination).wrap_err_with(|| {
            format!(
                "failed to remove existing pricing cache {}",
                destination.display()
            )
        })?;
        fs::rename(temporary, destination).wrap_err_with(|| {
            format!("failed to replace pricing cache {}", destination.display())
        })?;
    }
    Ok(())
}

/// Refresh pricing data from `LiteLLM`.
pub(super) fn fetch_pricing_cache() -> Result<PricingCache> {
    fetch_pricing_cache_from(
        LITELLM_PRICING_URL,
        HTTP_CONNECT_TIMEOUT,
        HTTP_REQUEST_TIMEOUT,
    )
}

/// Refresh pricing data from the provided URL.
pub(super) fn fetch_pricing_cache_from(
    url: &str,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<PricingCache> {
    let response = Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .wrap_err("failed to build HTTP client")?
        .get(url)
        .send()
        .wrap_err("failed to fetch LiteLLM pricing")?
        .error_for_status()
        .wrap_err("LiteLLM pricing request was unsuccessful")?;
    let dataset = response
        .json::<serde_json::Map<String, Value>>()
        .wrap_err("failed to decode LiteLLM pricing JSON")?;

    Ok(super::pricing_cache_from_dataset(dataset))
}
