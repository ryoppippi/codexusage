use codexusage::pricing::{
    CacheDecision, PricingLoadOptions, decide_cache_action, default_cache_path,
    load_pricing_catalog,
};
use std::fs;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

#[test]
fn missing_online_cache_triggers_refresh() {
    let temp = TempDir::new().expect("tempdir");
    let decision = decide_cache_action(
        &temp.path().join("missing.json"),
        SystemTime::UNIX_EPOCH + Duration::from_secs(100),
        Duration::from_mins(1),
        false,
        false,
    )
    .expect("decision");

    assert_eq!(decision, CacheDecision::Refresh);
}

#[test]
fn stale_cache_triggers_refresh() {
    let temp = TempDir::new().expect("tempdir");
    let cache_path = temp.path().join("pricing-cache.json");
    fs::write(&cache_path, "{}").expect("write cache");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    filetime::set_file_mtime(&cache_path, filetime::FileTime::from_system_time(old))
        .expect("set mtime");

    let decision = decide_cache_action(
        &cache_path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(10_000),
        Duration::from_mins(1),
        false,
        false,
    )
    .expect("decision");

    assert_eq!(decision, CacheDecision::Refresh);
}

#[test]
fn embedded_catalog_resolves_aliases_and_free_routes() {
    let catalog = load_pricing_catalog(&PricingLoadOptions {
        offline: true,
        force_refresh: false,
    })
    .expect("catalog");

    let alias = catalog.resolve("gpt-5.3-codex");
    assert!(alias.output_cost_per_mtoken > 0.0);

    let free = catalog.resolve("openrouter/openai/gpt-5:free");
    assert!(free.input_cost_per_mtoken.abs() < f64::EPSILON);
    assert!(free.cached_input_cost_per_mtoken.abs() < f64::EPSILON);
    assert!(free.output_cost_per_mtoken.abs() < f64::EPSILON);
}

#[test]
fn cache_path_prefers_user_cache_dir_shape() {
    let cache_path = default_cache_path();
    assert!(
        cache_path.to_string_lossy().contains("codexusage"),
        "cache path should include the application directory"
    );
}
