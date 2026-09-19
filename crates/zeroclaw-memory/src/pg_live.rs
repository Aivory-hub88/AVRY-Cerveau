//! A Postgres connection that heals itself.
//!
//! The task ledger, the capability graph and the skill-insight ledger each hold
//! ONE `postgres::Client`. That client never reconnects: once its connection
//! drops (a Postgres restart, a host reboot, a firewall killing an idle
//! connection) every later call fails with "connection closed" until the whole
//! daemon restarts. `PostgresMemory` does not have this problem because it uses
//! an `r2d2` pool; these three did.
//!
//! [`LiveClient::ready`] is called before each operation. A synchronous
//! `postgres::Client` does not notice a dead connection while nothing is using it
//! (nothing reads its socket), so `is_closed()` alone is not enough: a connection
//! that has been idle for a moment is verified with a trivial query first, and
//! reconnected if that fails. Back-to-back operations skip the check. The one call
//! that can still fail is one that lands within [`IDLE_CHECK_AFTER`] of a drop; the
//! client has noticed by the next call, which reconnects. A failed reconnect
//! returns an error and the next call simply tries again, so an outage that
//! outlasts one call heals as soon as Postgres is back.
//!
//! It must only be used from a plain OS thread (every call site already runs
//! inside `run_on_os_thread`): dropping the old client and connecting a new one
//! both call `Runtime::block_on` internally.

use anyhow::{Context, Result};
use postgres::{Client, NoTls};
use std::time::{Duration, Instant};

/// A connection unused for longer than this is checked before use. Small on
/// purpose: the check costs one round trip (~100 µs on the same host) and the
/// ledger is called from tool executions, not in a hot loop.
const IDLE_CHECK_AFTER: Duration = Duration::from_secs(1);

pub(crate) struct LiveClient {
    inner: Client,
    url: String,
    last_used: Instant,
}

impl LiveClient {
    pub(crate) fn new(inner: Client, url: String) -> Self {
        Self {
            inner,
            url,
            last_used: Instant::now(),
        }
    }

    /// A client that is safe to run a statement on, reconnecting if it must.
    pub(crate) fn ready(&mut self) -> Result<&mut Client> {
        if !self.healthy() {
            let fresh = Client::connect(&self.url, NoTls)
                .context("reconnect to Postgres after the connection was lost")?;
            // Replaces (and drops) the dead client, on this plain OS thread.
            self.inner = fresh;
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success),
                "postgres connection was lost; reconnected"
            );
        }
        self.last_used = Instant::now();
        Ok(&mut self.inner)
    }

    fn healthy(&mut self) -> bool {
        if self.inner.is_closed() {
            return false;
        }
        if self.last_used.elapsed() > IDLE_CHECK_AFTER {
            return self.inner.simple_query("SELECT 1").is_ok();
        }
        true
    }
}
