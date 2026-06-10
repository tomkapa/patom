//! Per-org monthly spend budget.
//!
//! Patom caps *turns* per DAG ([`crate::runtime::dag`]) and records per-turn
//! token counts (`turn_metrics`), but nothing converts that to money or stops
//! spend at a limit. This module adds a per-org monthly budget in **micro-USD**
//! (`1e-6` USD), enforced at two seams:
//!
//! - **Admission** — [`BillingService::check_or_fail_for_user`] gates a new root
//!   prompt at the HTTP boundary (tenant-scoped, RLS) so an over-cap org gets an
//!   immediate `429`.
//! - **Per-turn** — [`BillingService::check_or_fail`] gates each provider call in
//!   the worker (privileged) so a long-running DAG stops once it crosses the cap.
//!
//! Token cost is **post-paid**: real counts are known only after the provider
//! responds, so enforcement is *check-before / settle-after*. The gate reads a
//! stale total; [`BillingService::settle`] adds the actual turn cost atomically
//! afterwards. The single-turn overrun this allows is bounded — see
//! [`limits::MAX_SINGLE_TURN_COST_MICROS`].

pub mod error;
pub mod limits;
pub mod pricing;
pub mod service;
pub mod types;

pub use error::BillingError;
pub use pricing::{price_for, turn_cost};
pub use service::{BillingConfig, BillingService, PgBillingService, SharedBillingService};
pub use types::{
    BillingPeriod, CostMicros, MicroUsdPerMtok, MonthlyCapMicros, Price, WarnThresholdBps,
};
