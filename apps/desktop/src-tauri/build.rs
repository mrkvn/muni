// Build-time identity baking.
//
// A release `.app` launched from Finder inherits no shell env, so any
// per-flavor identity (staging keychain isolation, telemetry keys) must be
// compiled in. `scripts/install-staging` /
// `build-prod`/`install-staging` export these `MUNI_BAKED_*` vars before `tauri build`;
// we forward each present one into the crate via `rustc-env` so
// `option_env!` can read it. `rerun-if-env-changed` makes cargo recompile
// when a value flips, so a warm `target/` never bakes a stale id.
const BAKED_ENV_VARS: &[&str] = &[
    "MUNI_BAKED_KEYCHAIN_SUFFIX",
    // Feature 033 — telemetry identities baked prod-only (left unset in
    // dev/staging so those builds resolve an empty DSN/key and stay silent).
    // The Sentry DSN + PostHog `phc_` key are client-embeddable, not secrets.
    "MUNI_BAKED_SENTRY_DSN",
    "MUNI_BAKED_POSTHOG_KEY",
];

fn main() {
    for var in BAKED_ENV_VARS {
        println!("cargo:rerun-if-env-changed={var}");
        if let Ok(value) = std::env::var(var) {
            // Cargo does not pass arbitrary build-env vars to rustc, so an
            // explicit re-emit is required for `option_env!` to see them.
            println!("cargo:rustc-env={var}={value}");
        }
    }
    tauri_build::build()
}
