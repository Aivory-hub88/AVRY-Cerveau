//! Automatic, debounced mirroring of a tenant's durable facts into their
//! knowledge graph (cognee-rs).
//!
//! Until now the graph only filled when the *model* chose to call
//! `graph_remember`, and in production it never did (7 documents in total, none
//! for days). Post-turn consolidation already distills each tenant turn into
//! the durable facts worth keeping; this module forwards those to the graph so
//! persistence no longer depends on model behaviour.
//!
//! Cost shape (measured on the sidecar): `add` is ~0.1 s, but `cognify` is an
//! LLM pipeline (~40 s). So every fact is `add`ed immediately and `cognify` is
//! debounced per `(tenant, agent_type)`: a burst of facts inside the window
//! costs one extraction. Everything here is best-effort and fire-and-forget --
//! a graph outage must never affect a turn.

use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;
use zeroclaw_config::schema::CogneeConfig;

/// `(tenant, agent_type)` keys with a `cognify` already scheduled.
fn scheduled() -> &'static Mutex<HashSet<String>> {
    static SCHEDULED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SCHEDULED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Forward one durable `text` to the tenant's graph. Returns immediately; the
/// work runs on a spawned task. No-op unless `[cognee]` has both `enabled` and
/// `auto_ingest` set.
pub fn enqueue_ingest(cfg: &CogneeConfig, tenant_id: &str, agent_type: &str, text: &str) {
    if !(cfg.enabled && cfg.auto_ingest) || text.trim().is_empty() {
        return;
    }
    enqueue_ingest_after(
        cfg.clone(),
        tenant_id.to_string(),
        agent_type.to_string(),
        text.to_string(),
        Duration::from_secs(cfg.ingest_debounce_secs),
    );
}

fn enqueue_ingest_after(
    cfg: CogneeConfig,
    tenant_id: String,
    agent_type: String,
    text: String,
    debounce: Duration,
) {
    zeroclaw_spawn::spawn!(async move {
        if let Err(e) =
            zeroclaw_tools::graph_memory::add_fact(&cfg, &tenant_id, &agent_type, &text).await
        {
            note_failure("add", &e);
            return;
        }
        let key = format!("{tenant_id}\u{1f}{agent_type}");
        // A `cognify` is already waiting for this tenant: it will cover this fact.
        if !scheduled().lock().insert(key.clone()) {
            return;
        }
        tokio::time::sleep(debounce).await;
        // Release before running so a fact added *during* extraction schedules
        // the next run instead of being missed.
        scheduled().lock().remove(&key);
        if let Err(e) = zeroclaw_tools::graph_memory::cognify(&cfg, &tenant_id, &agent_type).await {
            note_failure("cognify", &e);
        }
    });
}

fn note_failure(stage: &str, error: &anyhow::Error) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({"stage": stage, "error": format!("{error:#}")})),
        "cerveau: automatic graph ingest failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn sidecar(adds: u64, cognifies: u64) -> MockServer {
        // The first shared-client build is slow in a debug build and would skew
        // the debounce timing these tests measure.
        zeroclaw_tools::graph_memory::warm_client();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/add"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(adds)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/cognify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(cognifies)
            .mount(&server)
            .await;
        server
    }

    fn cfg(server: &MockServer) -> CogneeConfig {
        CogneeConfig {
            enabled: true,
            auto_ingest: true,
            base_url: server.uri(),
            ..CogneeConfig::default()
        }
    }

    #[tokio::test]
    async fn burst_of_facts_costs_one_extraction() {
        // 3 facts inside the debounce window: 3 cheap adds, ONE cognify.
        let server = sidecar(3, 1).await;
        let cfg = cfg(&server);
        for fact in ["fact one", "fact two", "fact three"] {
            enqueue_ingest_after(
                cfg.clone(),
                "burst-tenant".into(),
                "leads_qualifier".into(),
                fact.into(),
                Duration::from_millis(400),
            );
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        server.verify().await;
    }

    #[tokio::test]
    async fn a_fact_after_the_window_schedules_the_next_extraction() {
        let server = sidecar(2, 2).await;
        let cfg = cfg(&server);
        enqueue_ingest_after(
            cfg.clone(),
            "later-tenant".into(),
            "leads_qualifier".into(),
            "early".into(),
            Duration::from_millis(150),
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        enqueue_ingest_after(
            cfg,
            "later-tenant".into(),
            "leads_qualifier".into(),
            "late".into(),
            Duration::from_millis(150),
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        for r in server.received_requests().await.unwrap() {
            eprintln!("DBG {} {}", r.method, r.url.path());
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn disabled_or_blank_ingest_makes_no_requests() {
        let server = sidecar(0, 0).await;
        // auto_ingest off
        let off = CogneeConfig {
            auto_ingest: false,
            ..cfg(&server)
        };
        enqueue_ingest(&off, "t", "a", "a real fact");
        // cognee disabled
        let disabled = CogneeConfig {
            enabled: false,
            ..cfg(&server)
        };
        enqueue_ingest(&disabled, "t", "a", "a real fact");
        // blank text
        enqueue_ingest(&cfg(&server), "t", "a", "   ");
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.verify().await;
    }

    #[tokio::test]
    async fn a_dead_sidecar_is_swallowed() {
        // Best-effort: no panic, no hang, nothing to observe.
        let dead = CogneeConfig {
            enabled: true,
            auto_ingest: true,
            base_url: "http://127.0.0.1:1".into(),
            ..CogneeConfig::default()
        };
        enqueue_ingest_after(
            dead,
            "dead".into(),
            "a".into(),
            "fact".into(),
            Duration::from_millis(10),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
