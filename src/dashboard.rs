use std::sync::LazyLock;

use axum::{extract::Request, response::Response};
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
struct DashboardAssets;

static DASHBOARD: LazyLock<EmbeddedSpa<DashboardAssets>> = LazyLock::new(|| {
    EmbeddedSpa::new(EmbeddedSpaConfig::default())
        .expect("frontend/dist must contain a valid index.html")
});

pub async fn serve_spa(request: Request) -> Response {
    DASHBOARD.serve(request)
}

#[cfg(test)]
mod tests {
    const INDEX: &str = include_str!("../frontend/index.html");
    const APP: &str = include_str!("../frontend/src/main.js");

    #[test]
    fn dashboard_uses_real_routing_and_statistics_endpoints() {
        assert!(APP.contains("api('/api/routing')"));
        assert!(APP.contains("/api/routing/stats?model="));
        assert!(APP.contains("['calls','Calls']"));
        assert!(APP.contains("['total_tokens','Total tokens']"));
        assert!(APP.contains("['input_tokens','Input tokens']"));
        assert!(APP.contains("['output_tokens','Output tokens']"));
        assert!(APP.contains("['1m','1 month']"));
        assert!(APP.contains("['all','All time']"));
    }

    #[test]
    fn dashboard_shows_only_recorded_routing_concepts() {
        assert!(APP.contains("Route: Fallback exhausted"));
        for expected in [
            "Served model",
            "Layer",
            "Strategy",
            "Backend",
            "Key",
            "Upstream model",
            "Circuit",
            "Next probe",
        ] {
            assert!(INDEX.contains(expected), "missing {expected}");
        }
        let source = format!("{INDEX}\n{APP}");
        for absent in [
            "Protocol",
            "Degraded",
            "Bypassed",
            "progress-bar",
            "bar-fill",
            "Share",
            "Last updated",
            "Layered multi-provider gateway",
        ] {
            assert!(!source.contains(absent), "unexpected {absent}");
        }
    }
}
