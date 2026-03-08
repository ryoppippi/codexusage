//! Pricing cache and refresh helpers.

/// Pricing cache IO and refresh helpers.
#[path = "pricing/cache.rs"]
mod cache;

pub use cache::{decide_cache_action, default_cache_path};

use cache::{CACHE_TTL, cache_age, fetch_pricing_cache, read_cache, write_cache};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Per-model pricing in USD per million tokens.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Pricing {
    /// Non-cached input price.
    pub input_cost_per_mtoken: f64,
    /// Cached input price.
    pub cached_input_cost_per_mtoken: f64,
    /// Output price.
    pub output_cost_per_mtoken: f64,
}

/// Cached pricing payload persisted on disk.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PricingCache {
    /// Timestamp of the last successful refresh.
    pub refreshed_at_epoch_seconds: i64,
    /// Model pricing table.
    pub models: BTreeMap<String, Pricing>,
}

/// High-level cache action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheDecision {
    /// Use the existing cache as-is.
    UseCache,
    /// Refresh from the network.
    Refresh,
}

/// Pricing load options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PricingLoadOptions {
    /// Disable network usage.
    pub offline: bool,
    /// Force live refresh.
    pub force_refresh: bool,
}

/// Resolved pricing data for a run.
#[derive(Clone, Debug, Default)]
pub struct PricingCatalog {
    /// Known pricing entries keyed by model name.
    models: HashMap<String, Pricing>,
}

impl PricingCatalog {
    /// Resolve pricing for a model using aliases and provider prefixes.
    #[must_use]
    pub fn resolve(&self, model: &str) -> Pricing {
        let normalized = model.trim();
        if normalized.eq_ignore_ascii_case("openrouter/free")
            || (normalized.to_ascii_lowercase().starts_with("openrouter/")
                && normalized.to_ascii_lowercase().ends_with(":free"))
        {
            return Pricing::default();
        }

        for candidate in resolution_candidates(normalized) {
            if let Some(pricing) = self.models.get(&candidate) {
                return pricing.clone();
            }
        }

        Pricing::default()
    }
}

/// Push a candidate string only once while preserving insertion order.
fn push_unique(candidates: &mut Vec<String>, candidate: String) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

/// Provider prefixes that can appear in Codex pricing datasets.
const PROVIDER_PREFIXES: [&str; 3] = ["openrouter/openai/", "openai/", "azure/"];

/// Build all lookup candidates for one model string.
fn resolution_candidates(model: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique(&mut candidates, model.to_string());
    if let Some(alias) = alias_for(model) {
        push_unique(&mut candidates, alias.to_string());
    }
    if let Some((prefix, base_model)) = split_provider_prefix(model) {
        push_unique(&mut candidates, base_model.to_string());
        if let Some(alias) = alias_for(base_model) {
            push_unique(&mut candidates, format!("{prefix}{alias}"));
            push_unique(&mut candidates, alias.to_string());
        }
    }

    for candidate in candidates.clone() {
        if split_provider_prefix(&candidate).is_some() {
            continue;
        }
        push_unique(&mut candidates, format!("openai/{candidate}"));
        push_unique(&mut candidates, format!("azure/{candidate}"));
        push_unique(&mut candidates, format!("openrouter/openai/{candidate}"));
    }

    candidates
}

/// Split a model string into a known provider prefix and the base model name.
fn split_provider_prefix(model: &str) -> Option<(&'static str, &str)> {
    PROVIDER_PREFIXES.iter().find_map(|prefix| {
        model
            .strip_prefix(prefix)
            .map(|base_model| (*prefix, base_model))
    })
}

/// Load pricing with cache + refresh behavior.
///
/// # Errors
///
/// Returns an error when a fresh pricing cache cannot be persisted after a successful refresh or
/// when an unreadable cache file cannot be recovered by refreshing.
pub fn load_pricing_catalog(options: &PricingLoadOptions) -> Result<PricingCatalog> {
    load_pricing_catalog_with(&default_cache_path(), *options, fetch_pricing_cache)
}

/// Load pricing with an explicit cache path and refresher.
fn load_pricing_catalog_with<F>(
    cache_path: &Path,
    options: PricingLoadOptions,
    fetcher: F,
) -> Result<PricingCatalog>
where
    F: Fn() -> Result<PricingCache>,
{
    let now = SystemTime::now();
    let (cache, cache_error) = match read_cache(cache_path) {
        Ok(cache) => (Some(cache), None),
        Err(error) if cache_path.exists() => (None, Some(error)),
        Err(_) => (None, None),
    };
    let decision = if options.offline {
        CacheDecision::UseCache
    } else if options.force_refresh {
        CacheDecision::Refresh
    } else if cache
        .as_ref()
        .and_then(|cache| cache_age(cache, now))
        .is_some_and(|age| age <= CACHE_TTL)
    {
        CacheDecision::UseCache
    } else {
        CacheDecision::Refresh
    };

    let embedded = embedded_pricing();
    match decision {
        CacheDecision::UseCache => match (cache.as_ref(), cache_error) {
            (Some(cache), _) => Ok(catalog_from_cache(Some(cache), &embedded)),
            (None, Some(_)) if options.offline => Ok(catalog_from_cache(None, &embedded)),
            (None, Some(error)) => refresh_after_cache_error(cache_path, &embedded, fetcher, error),
            (None, None) => Ok(catalog_from_cache(None, &embedded)),
        },
        CacheDecision::Refresh => match fetcher() {
            Ok(fresh) => {
                write_cache(cache_path, &fresh)?;
                Ok(catalog_from_cache(Some(&fresh), &embedded))
            }
            Err(error) if options.force_refresh => Err(error),
            Err(error) => {
                fallback_after_refresh_failure(cache.as_ref(), &embedded, cache_error, error)
            }
        },
    }
}

/// Refresh pricing after a cache file was present but unreadable.
fn refresh_after_cache_error<F>(
    cache_path: &Path,
    embedded: &BTreeMap<String, Pricing>,
    fetcher: F,
    cache_error: eyre::Report,
) -> Result<PricingCatalog>
where
    F: Fn() -> Result<PricingCache>,
{
    match fetcher() {
        Ok(fresh) => {
            write_cache(cache_path, &fresh)?;
            Ok(catalog_from_cache(Some(&fresh), embedded))
        }
        Err(refresh_error) => Err(refresh_error)
            .wrap_err("failed to refresh pricing after unreadable fresh cache")
            .wrap_err(cache_error),
    }
}

/// Fallback after a refresh attempt failed.
fn fallback_after_refresh_failure(
    cache: Option<&PricingCache>,
    embedded: &BTreeMap<String, Pricing>,
    cache_error: Option<eyre::Report>,
    refresh_error: eyre::Report,
) -> Result<PricingCatalog> {
    match (cache, cache_error) {
        (Some(cache), _) => Ok(catalog_from_cache(Some(cache), embedded)),
        (None, Some(cache_error)) => Err(refresh_error)
            .wrap_err("failed to refresh pricing after unreadable cache")
            .wrap_err(cache_error),
        (None, None) => Ok(catalog_from_cache(None, embedded)),
    }
}

/// Create a catalog from embedded + cached data.
fn catalog_from_cache(
    cache: Option<&PricingCache>,
    embedded: &BTreeMap<String, Pricing>,
) -> PricingCatalog {
    let mut models = embedded
        .iter()
        .map(|(model, pricing)| (model.clone(), pricing.clone()))
        .collect::<HashMap<_, _>>();
    if let Some(cache) = cache {
        for (model, pricing) in &cache.models {
            models.insert(model.clone(), pricing.clone());
        }
    }
    PricingCatalog { models }
}

/// Minimal embedded pricing used for tests and offline fallback.
fn embedded_pricing() -> BTreeMap<String, Pricing> {
    BTreeMap::from([
        (
            "gpt-5".to_string(),
            Pricing {
                input_cost_per_mtoken: 1.25,
                cached_input_cost_per_mtoken: 0.125,
                output_cost_per_mtoken: 10.0,
            },
        ),
        (
            "gpt-5-codex".to_string(),
            Pricing {
                input_cost_per_mtoken: 1.25,
                cached_input_cost_per_mtoken: 0.125,
                output_cost_per_mtoken: 10.0,
            },
        ),
        (
            "gpt-5.2-codex".to_string(),
            Pricing {
                input_cost_per_mtoken: 1.25,
                cached_input_cost_per_mtoken: 0.125,
                output_cost_per_mtoken: 10.0,
            },
        ),
    ])
}

/// Resolve known model aliases.
fn alias_for(model: &str) -> Option<&'static str> {
    match model {
        "gpt-5-codex" => Some("gpt-5"),
        "gpt-5.3-codex" => Some("gpt-5.2-codex"),
        _ => None,
    }
}

/// Convert the raw `LiteLLM` payload into a persisted cache structure.
fn pricing_cache_from_dataset(dataset: serde_json::Map<String, Value>) -> PricingCache {
    let mut models = BTreeMap::new();
    for (model, data) in dataset {
        let Some(object) = data.as_object() else {
            continue;
        };
        let input = object
            .get("input_cost_per_token")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let output = object
            .get("output_cost_per_token")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let cached = object
            .get("cache_read_input_token_cost")
            .and_then(Value::as_f64)
            .unwrap_or(input);
        if input == 0.0 && output == 0.0 && cached == 0.0 {
            continue;
        }
        models.insert(
            model,
            Pricing {
                input_cost_per_mtoken: input * 1_000_000.0,
                cached_input_cost_per_mtoken: cached * 1_000_000.0,
                output_cost_per_mtoken: output * 1_000_000.0,
            },
        );
    }

    let refreshed_at_epoch_seconds = i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs(),
    )
    .unwrap_or(i64::MAX);

    PricingCache {
        refreshed_at_epoch_seconds,
        models,
    }
}

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod tests {
    use super::*;
    use crate::pricing::cache::fetch_pricing_cache_from;
    use std::cell::Cell;
    use std::fs;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn catalog_from_cache_overrides_embedded_pricing() {
        let embedded = embedded_pricing();
        let cache = PricingCache {
            refreshed_at_epoch_seconds: 1,
            models: BTreeMap::from([(
                "gpt-5".to_string(),
                Pricing {
                    input_cost_per_mtoken: 9.0,
                    cached_input_cost_per_mtoken: 8.0,
                    output_cost_per_mtoken: 7.0,
                },
            )]),
        };

        let catalog = catalog_from_cache(Some(&cache), &embedded);
        let pricing = catalog.resolve("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 9.0).abs() < f64::EPSILON);
        assert!((pricing.cached_input_cost_per_mtoken - 8.0).abs() < f64::EPSILON);
        assert!((pricing.output_cost_per_mtoken - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn write_and_read_cache_round_trip() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        let cache = PricingCache {
            refreshed_at_epoch_seconds: 123,
            models: BTreeMap::from([(
                "gpt-5".to_string(),
                Pricing {
                    input_cost_per_mtoken: 1.25,
                    cached_input_cost_per_mtoken: 0.125,
                    output_cost_per_mtoken: 10.0,
                },
            )]),
        };

        write_cache(&cache_path, &cache).expect("write cache");
        let round_trip = read_cache(&cache_path).expect("read cache");
        assert_eq!(round_trip, cache);
    }

    #[test]
    fn write_cache_overwrites_existing_destination() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        write_cache(
            &cache_path,
            &PricingCache {
                refreshed_at_epoch_seconds: 1,
                models: BTreeMap::new(),
            },
        )
        .expect("first write");
        write_cache(
            &cache_path,
            &PricingCache {
                refreshed_at_epoch_seconds: 2,
                models: BTreeMap::from([(
                    "gpt-5".to_string(),
                    Pricing {
                        input_cost_per_mtoken: 1.0,
                        cached_input_cost_per_mtoken: 1.0,
                        output_cost_per_mtoken: 1.0,
                    },
                )]),
            },
        )
        .expect("second write");

        let cache = read_cache(&cache_path).expect("cache");
        assert_eq!(cache.refreshed_at_epoch_seconds, 2);
        assert!(cache.models.contains_key("gpt-5"));
    }

    #[test]
    fn embedded_pricing_contains_expected_alias_targets() {
        let embedded = embedded_pricing();
        assert!(embedded.contains_key("gpt-5"));
        assert!(embedded.contains_key("gpt-5-codex"));
        assert!(embedded.contains_key("gpt-5.2-codex"));
        assert_eq!(alias_for("gpt-5.3-codex"), Some("gpt-5.2-codex"));
    }

    #[test]
    fn resolve_uses_provider_prefix_match() {
        let catalog = PricingCatalog {
            models: HashMap::from([(
                "openai/gpt-5".to_string(),
                Pricing {
                    input_cost_per_mtoken: 1.0,
                    cached_input_cost_per_mtoken: 0.5,
                    output_cost_per_mtoken: 2.0,
                },
            )]),
        };

        let pricing = catalog.resolve("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 1.0).abs() < f64::EPSILON);
        assert!((pricing.cached_input_cost_per_mtoken - 0.5).abs() < f64::EPSILON);
        assert!((pricing.output_cost_per_mtoken - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn resolve_does_not_guess_using_substring_matches() {
        let catalog = PricingCatalog {
            models: HashMap::from([(
                "gpt-5".to_string(),
                Pricing {
                    input_cost_per_mtoken: 1.0,
                    cached_input_cost_per_mtoken: 0.5,
                    output_cost_per_mtoken: 2.0,
                },
            )]),
        };

        let pricing = catalog.resolve("gpt-5-mini");
        assert!(pricing.input_cost_per_mtoken.abs() < f64::EPSILON);
        assert!(pricing.cached_input_cost_per_mtoken.abs() < f64::EPSILON);
        assert!(pricing.output_cost_per_mtoken.abs() < f64::EPSILON);
    }

    #[test]
    fn resolve_applies_provider_prefix_to_aliases() {
        let catalog = PricingCatalog {
            models: HashMap::from([(
                "openai/gpt-5".to_string(),
                Pricing {
                    input_cost_per_mtoken: 3.0,
                    cached_input_cost_per_mtoken: 2.0,
                    output_cost_per_mtoken: 1.0,
                },
            )]),
        };

        let pricing = catalog.resolve("gpt-5-codex");
        assert!((pricing.input_cost_per_mtoken - 3.0).abs() < f64::EPSILON);
        assert!((pricing.cached_input_cost_per_mtoken - 2.0).abs() < f64::EPSILON);
        assert!((pricing.output_cost_per_mtoken - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn resolve_applies_aliases_to_provider_qualified_models() {
        let expected_openai = Pricing {
            input_cost_per_mtoken: 3.0,
            cached_input_cost_per_mtoken: 2.0,
            output_cost_per_mtoken: 1.0,
        };
        let expected_openrouter = Pricing {
            input_cost_per_mtoken: 6.0,
            cached_input_cost_per_mtoken: 5.0,
            output_cost_per_mtoken: 4.0,
        };
        let catalog = PricingCatalog {
            models: HashMap::from([
                ("openai/gpt-5".to_string(), expected_openai.clone()),
                (
                    "openrouter/openai/gpt-5.2-codex".to_string(),
                    expected_openrouter.clone(),
                ),
            ]),
        };

        assert_eq!(catalog.resolve("openai/gpt-5-codex"), expected_openai);
        assert_eq!(
            catalog.resolve("openrouter/openai/gpt-5.3-codex"),
            expected_openrouter
        );
    }

    #[test]
    fn pricing_cache_from_dataset_filters_invalid_entries() {
        let cache = pricing_cache_from_dataset(serde_json::Map::from_iter([
            ("invalid".to_string(), Value::String("nope".to_string())),
            (
                "gpt-5".to_string(),
                serde_json::json!({
                    "input_cost_per_token": 1.25e-6,
                    "output_cost_per_token": 1.0e-5,
                    "cache_read_input_token_cost": 1.25e-7
                }),
            ),
            (
                "free".to_string(),
                serde_json::json!({
                    "input_cost_per_token": 0.0,
                    "output_cost_per_token": 0.0,
                    "cache_read_input_token_cost": 0.0
                }),
            ),
        ]));

        assert_eq!(cache.models.len(), 1);
        let pricing = cache.models.get("gpt-5").expect("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 1.25).abs() < f64::EPSILON);
        assert!((pricing.cached_input_cost_per_mtoken - 0.125).abs() < f64::EPSILON);
        assert!((pricing.output_cost_per_mtoken - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn load_pricing_catalog_with_uses_fresh_cache_without_refresh() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        let cache = PricingCache {
            refreshed_at_epoch_seconds: i64::try_from(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("duration")
                    .as_secs(),
            )
            .expect("timestamp fits in i64"),
            models: BTreeMap::from([(
                "gpt-5".to_string(),
                Pricing {
                    input_cost_per_mtoken: 4.0,
                    cached_input_cost_per_mtoken: 3.0,
                    output_cost_per_mtoken: 2.0,
                },
            )]),
        };
        write_cache(&cache_path, &cache).expect("write cache");
        let catalog = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: false,
            },
            || -> Result<PricingCache> { unreachable!("fresh cache should not refresh") },
        )
        .expect("catalog");

        let pricing = catalog.resolve("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn load_pricing_catalog_with_refreshes_and_persists_new_cache() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        let catalog = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: true,
            },
            || {
                Ok(PricingCache {
                    refreshed_at_epoch_seconds: 99,
                    models: BTreeMap::from([(
                        "gpt-5".to_string(),
                        Pricing {
                            input_cost_per_mtoken: 6.0,
                            cached_input_cost_per_mtoken: 5.0,
                            output_cost_per_mtoken: 4.0,
                        },
                    )]),
                })
            },
        )
        .expect("catalog");

        let pricing = catalog.resolve("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 6.0).abs() < f64::EPSILON);

        let persisted = read_cache(&cache_path).expect("persisted cache");
        assert_eq!(persisted.refreshed_at_epoch_seconds, 99);
    }

    #[test]
    fn load_pricing_catalog_with_falls_back_to_existing_cache_on_refresh_failure() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        write_cache(
            &cache_path,
            &PricingCache {
                refreshed_at_epoch_seconds: 10,
                models: BTreeMap::from([(
                    "gpt-5".to_string(),
                    Pricing {
                        input_cost_per_mtoken: 2.0,
                        cached_input_cost_per_mtoken: 1.0,
                        output_cost_per_mtoken: 3.0,
                    },
                )]),
            },
        )
        .expect("write cache");
        filetime::set_file_mtime(
            &cache_path,
            filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH + Duration::from_hours(1)),
        )
        .expect("set mtime");

        let catalog = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: false,
            },
            || Err(eyre::eyre!("boom")),
        )
        .expect("catalog");

        let pricing = catalog.resolve("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn load_pricing_catalog_with_force_refresh_fails_when_fetch_fails() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        let error = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: true,
            },
            || Err(eyre::eyre!("boom")),
        )
        .expect_err("forced refresh should surface fetch errors");

        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn load_pricing_catalog_with_returns_embedded_pricing_when_refresh_fails_without_cache() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("missing-cache.json");
        let catalog = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: false,
            },
            || Err(eyre::eyre!("boom")),
        )
        .expect("catalog");

        let pricing = catalog.resolve("gpt-5");
        assert!((pricing.input_cost_per_mtoken - 1.25).abs() < f64::EPSILON);
        assert!((pricing.cached_input_cost_per_mtoken - 0.125).abs() < f64::EPSILON);
    }

    #[test]
    fn load_pricing_catalog_with_refreshes_when_fresh_cache_is_unreadable() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        fs::write(&cache_path, "{not-json").expect("write invalid cache");
        let refresh_calls = Cell::new(0);

        let catalog = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: false,
            },
            || {
                refresh_calls.set(refresh_calls.get() + 1);
                Ok(PricingCache {
                    refreshed_at_epoch_seconds: 42,
                    models: BTreeMap::from([(
                        "fresh-model".to_string(),
                        Pricing {
                            input_cost_per_mtoken: 7.0,
                            cached_input_cost_per_mtoken: 6.0,
                            output_cost_per_mtoken: 5.0,
                        },
                    )]),
                })
            },
        )
        .expect("catalog");

        assert_eq!(refresh_calls.get(), 1);
        assert_eq!(
            catalog.resolve("fresh-model"),
            Pricing {
                input_cost_per_mtoken: 7.0,
                cached_input_cost_per_mtoken: 6.0,
                output_cost_per_mtoken: 5.0,
            }
        );
        assert!(
            read_cache(&cache_path)
                .expect("cache")
                .models
                .contains_key("fresh-model")
        );
    }

    #[test]
    fn load_pricing_catalog_with_errors_when_unreadable_cache_cannot_refresh() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        fs::write(&cache_path, "{not-json").expect("write invalid cache");

        let error = load_pricing_catalog_with(
            &cache_path,
            PricingLoadOptions {
                offline: false,
                force_refresh: false,
            },
            || Err(eyre::eyre!("boom")),
        )
        .expect_err("refresh failure should surface when cache is unreadable");

        let message = format!("{error:#}");
        assert!(message.contains("boom"));
        assert!(message.contains("failed to parse pricing cache"));
    }

    #[test]
    fn fetch_pricing_cache_from_times_out_on_unresponsive_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept");
            thread::sleep(Duration::from_millis(200));
        });

        let started = Instant::now();
        let error = fetch_pricing_cache_from(
            &format!("http://{address}/pricing.json"),
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .expect_err("request should time out");

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{error:#}").contains("failed to fetch LiteLLM pricing"));
        server.join().expect("join");
    }
}
