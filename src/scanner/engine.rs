// Two-Lane Priority 10,000-Home Concurrent Batch Scan Engine & Micro-Batch Persistence Writer
//
// Architectural highlights:
// - Dedicated P0 Interactive Lane (`interactive_reserved_permits`, default 32) isolated from
//   the P1 Bulk Worker Pool so single-address web users never queue behind a 10,000-home batch.
// - Bounded concurrency (`max_concurrent_scans`, default 256) and bounded queue (`max_queued_homes`,
//   default 50,000) preventing file-descriptor or memory exhaustion.
// - Within-batch AND cross-batch canonical slug singleflight deduplication (`inflight_slugs`).
// - Background Micro-Batch Persistence Task coalescing completed scans over `mpsc::channel` into
//   batched DB transactions (`db_flush_batch_size = 200`, `db_flush_interval_ms = 150`) with
//   deterministic completion barriers.

use crate::scanner::lazy_hook::PeriodicLazyEnqueueHook;
use crate::scanner::tile_cache::SpatialTileCache;
use crate::shared::{now_unix_ms, round_n, slugify, EnvConfig};
use anyhow::{anyhow, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};
use tokio::time::Instant;
use uuid::Uuid;

const STATUS_RUNNING: u8 = 0;
const STATUS_COMPLETED: u8 = 1;
const STATUS_CANCELLED: u8 = 2;

const ITEM_QUEUED: u8 = 0;
const ITEM_RUNNING: u8 = 1;
const ITEM_SUCCEEDED: u8 = 2;
const ITEM_CACHED: u8 = 3;
const ITEM_FAILED: u8 = 4;
const ITEM_CANCELLED: u8 = 5;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Pluggable home scan execution + batched persistence backend.
/// Allows `src/web/` to wire the 50-source property pipeline + `DbStore` batch upserts,
/// while integration tests exercise 10,000-home concurrency deterministically.
pub trait HomeScanRunner: Send + Sync + 'static {
    fn scan_one(&self, item: ScanTask) -> impl Future<Output = Result<ScanItemOutcome>> + Send;

    fn flush_batch(
        &self,
        outcomes: Vec<ScanItemOutcome>,
    ) -> impl Future<Output = Result<()>> + Send {
        async move {
            let _ = outcomes;
            Ok(())
        }
    }
}

trait ErasedHomeScanRunner: Send + Sync + 'static {
    fn scan_one_box<'a>(&'a self, item: ScanTask) -> BoxFuture<'a, Result<ScanItemOutcome>>;
    fn flush_batch_box<'a>(&'a self, outcomes: Vec<ScanItemOutcome>) -> BoxFuture<'a, Result<()>>;
}

struct RunnerAdapter<R: HomeScanRunner>(R);

impl<R: HomeScanRunner> ErasedHomeScanRunner for RunnerAdapter<R> {
    fn scan_one_box<'a>(&'a self, item: ScanTask) -> BoxFuture<'a, Result<ScanItemOutcome>> {
        Box::pin(self.0.scan_one(item))
    }

    fn flush_batch_box<'a>(&'a self, outcomes: Vec<ScanItemOutcome>) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.0.flush_batch(outcomes))
    }
}

/// Type-erased `HomeScanRunner` wrapper (default type parameter for `ScanEngine`).
#[derive(Clone)]
pub struct DynScanRunner {
    inner: Arc<dyn ErasedHomeScanRunner>,
}

impl DynScanRunner {
    pub fn new<R: HomeScanRunner>(runner: R) -> Self {
        Self {
            inner: Arc::new(RunnerAdapter(runner)),
        }
    }
}

impl HomeScanRunner for DynScanRunner {
    async fn scan_one(&self, item: ScanTask) -> Result<ScanItemOutcome> {
        self.inner.scan_one_box(item).await
    }

    async fn flush_batch(&self, outcomes: Vec<ScanItemOutcome>) -> Result<()> {
        self.inner.flush_batch_box(outcomes).await
    }
}

struct ClosureRunner<ScanFn, FlushFn> {
    scan_fn: ScanFn,
    flush_fn: FlushFn,
}

impl<ScanFn, ScanFut, FlushFn, FlushFut> HomeScanRunner for ClosureRunner<ScanFn, FlushFn>
where
    ScanFn: Fn(ScanTask) -> ScanFut + Send + Sync + 'static,
    ScanFut: Future<Output = Result<ScanItemOutcome>> + Send + 'static,
    FlushFn: Fn(Vec<ScanItemOutcome>) -> FlushFut + Send + Sync + 'static,
    FlushFut: Future<Output = Result<()>> + Send + 'static,
{
    fn scan_one(&self, item: ScanTask) -> impl Future<Output = Result<ScanItemOutcome>> + Send {
        (self.scan_fn)(item)
    }

    fn flush_batch(
        &self,
        outcomes: Vec<ScanItemOutcome>,
    ) -> impl Future<Output = Result<()>> + Send {
        (self.flush_fn)(outcomes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScanPriority {
    Interactive,
    #[default]
    Bulk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanTask {
    pub slug: String,
    pub address: String,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub zip: Option<String>,
    #[serde(default)]
    pub tract: Option<String>,
    #[serde(default)]
    pub force_refresh: bool,
    #[serde(default)]
    pub skip_ai: bool,
    #[serde(default)]
    pub priority: ScanPriority,
    #[serde(default)]
    pub metadata: Value,
}

impl ScanTask {
    pub fn new(address: impl Into<String>) -> Self {
        let address = collapse_addr_ws(&address.into());
        let slug = canonical_address_slug(&address);
        Self {
            slug,
            address,
            lat: None,
            lon: None,
            zip: None,
            tract: None,
            force_refresh: false,
            skip_ai: false,
            priority: ScanPriority::Bulk,
            metadata: Value::Null,
        }
    }

    pub fn interactive(address: impl Into<String>) -> Self {
        let mut t = Self::new(address);
        t.priority = ScanPriority::Interactive;
        t
    }

    pub fn with_coords(
        address: impl Into<String>,
        lat: f64,
        lon: f64,
        zip: Option<String>,
    ) -> Self {
        let mut t = Self::new(address);
        t.lat = Some(lat);
        t.lon = Some(lon);
        t.zip = zip;
        t
    }

    pub fn ensure_canonical_slug(&mut self) {
        if !is_clean_whitespace(&self.address) {
            self.address = collapse_addr_ws(&self.address);
        }
        if self.slug.trim().is_empty() {
            self.slug = canonical_address_slug(&self.address);
        } else if !is_already_canonical_slug(&self.slug) {
            self.slug = canonical_address_slug(&self.slug);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanItemOutcome {
    pub slug: String,
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overall_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_value: Option<f64>,
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub report_summary: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanEngineConfig {
    pub max_queued_homes: usize,
    pub max_concurrent_scans: usize,
    pub interactive_reserved_permits: usize,
    pub db_flush_batch_size: usize,
    pub db_flush_interval_ms: u64,
    pub max_batch_submission: usize,
}

impl Default for ScanEngineConfig {
    fn default() -> Self {
        let env_conc = std::env::var("SCAN_CONCURRENCY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n >= 2);
        Self {
            max_queued_homes: 50_000,
            max_concurrent_scans: env_conc.unwrap_or(256),
            interactive_reserved_permits: 32,
            db_flush_batch_size: 200,
            db_flush_interval_ms: 150,
            max_batch_submission: 20_000,
        }
    }
}

impl ScanEngineConfig {
    pub fn from_env(env: &EnvConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(c) = env
            .get("SCAN_CONCURRENCY")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 2)
        {
            cfg.max_concurrent_scans = c;
        }
        if let Some(q) = env
            .get("SCAN_MAX_QUEUED_HOMES")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 64)
        {
            cfg.max_queued_homes = q;
        }
        if let Some(p) = env
            .get("SCAN_INTERACTIVE_PERMITS")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 1)
        {
            cfg.interactive_reserved_permits = p;
        }
        if let Some(b) = env
            .get("SCAN_DB_FLUSH_BATCH_SIZE")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 1)
        {
            cfg.db_flush_batch_size = b;
        }
        if let Some(ms) = env
            .get("SCAN_DB_FLUSH_INTERVAL_MS")
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n >= 5)
        {
            cfg.db_flush_interval_ms = ms;
        }
        cfg
    }

    fn effective_lane_permits(&self) -> (usize, usize) {
        let total = self.max_concurrent_scans.max(2);
        let interactive = self
            .interactive_reserved_permits
            .max(1)
            .min(total.saturating_sub(1));
        let bulk = (total - interactive).max(1);
        (interactive, bulk)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchScanRequest {
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<ScanTask>,
    #[serde(default)]
    pub force_refresh: bool,
    #[serde(default)]
    pub skip_ai: bool,
    #[serde(default)]
    pub label: Option<String>,
}

impl BatchScanRequest {
    pub fn from_addresses(addresses: Vec<String>) -> Self {
        Self {
            addresses,
            tasks: Vec::new(),
            force_refresh: false,
            skip_ai: false,
            label: None,
        }
    }

    pub fn from_tasks(tasks: Vec<ScanTask>) -> Self {
        Self {
            addresses: Vec::new(),
            tasks,
            force_refresh: false,
            skip_ai: false,
            label: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchScanAccepted {
    pub ok: bool,
    pub batch_id: String,
    pub total_submitted: usize,
    pub unique_queued: usize,
    pub deduplicated_within_batch: usize,
    pub deduplicated_inflight: usize,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchScanItemStatus {
    pub index: usize,
    pub slug: String,
    pub address: String,
    pub status: String,
    pub cached: bool,
    pub deduplicated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overall_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchScanStatus {
    pub batch_id: String,
    pub status: String,
    pub total: usize,
    pub unique_queued: usize,
    pub completed: usize,
    pub cached: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub in_flight: usize,
    pub elapsed_ms: u64,
    pub throughput_homes_per_sec: f64,
    pub eta_sec: Option<f64>,
    pub offset: usize,
    pub limit: usize,
    pub items: Vec<BatchScanItemStatus>,
}

#[derive(Default)]
struct ItemSlotDetail {
    duration_ms: Option<u64>,
    overall_score: Option<f64>,
    estimated_value: Option<f64>,
    error: Option<String>,
}

struct BatchItemSlot {
    slug: String,
    address: String,
    deduplicated_initial: bool,
    state_code: AtomicU8,
    cached_flag: AtomicBool,
    deduplicated_flag: AtomicBool,
    detail: Mutex<ItemSlotDetail>,
}

pub struct ScanBatchState {
    pub batch_id: String,
    pub label: Option<String>,
    pub total: usize,
    pub unique_queued: AtomicUsize,
    pub started_at_ms: u64,
    finished_at_ms: AtomicU64,
    status_code: AtomicU8,
    cancelled: AtomicBool,
    counted_active: AtomicBool,
    completed: AtomicUsize,
    cached: AtomicUsize,
    succeeded: AtomicUsize,
    failed: AtomicUsize,
    cancelled_items: AtomicUsize,
    resolved_slots: AtomicUsize,
    in_flight: AtomicUsize,
    items: Vec<BatchItemSlot>,
    attached_inflights: Mutex<Vec<Arc<InflightSlug>>>,
    done_notify: Notify,
}

impl ScanBatchState {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn is_terminal(&self) -> bool {
        self.status_code.load(Ordering::Acquire) != STATUS_RUNNING
    }

    fn status_str(&self) -> &'static str {
        match self.status_code.load(Ordering::Acquire) {
            STATUS_COMPLETED => "completed",
            STATUS_CANCELLED => "cancelled",
            _ => "running",
        }
    }
}

#[derive(Clone)]
struct SubscriberSlot {
    batch: Arc<ScanBatchState>,
    slot_index: usize,
    is_primary: bool,
}

struct InflightSlugState {
    subscribers: Vec<SubscriberSlot>,
    sealed: bool,
}

struct InflightSlug {
    slug: String,
    state: Mutex<InflightSlugState>,
    cancel_notify: Notify,
}

impl InflightSlug {
    fn new(slug: String, subscribers: Vec<SubscriberSlot>) -> Arc<Self> {
        Arc::new(Self {
            slug,
            state: Mutex::new(InflightSlugState {
                subscribers,
                sealed: false,
            }),
            cancel_notify: Notify::new(),
        })
    }

    fn all_subscribers_cancelled(&self) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !st.subscribers.is_empty() && st.subscribers.iter().all(|s| s.batch.is_cancelled())
    }

    fn mark_slots_running_and_collect_batches(&self) -> Vec<Arc<ScanBatchState>> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = HashSet::new();
        let mut unique_batches = Vec::new();
        for sub in &st.subscribers {
            if sub.batch.is_cancelled() {
                continue;
            }
            sub.batch.items[sub.slot_index]
                .state_code
                .compare_exchange(
                    ITEM_QUEUED,
                    ITEM_RUNNING,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .ok();
            if seen.insert(sub.batch.batch_id.clone()) {
                unique_batches.push(Arc::clone(&sub.batch));
            }
        }
        unique_batches
    }

    fn seal_and_take_subscribers(&self) -> Vec<SubscriberSlot> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.sealed = true;
        std::mem::take(&mut st.subscribers)
    }
}

struct QueuedWorkItem {
    task: ScanTask,
    inflight: Arc<InflightSlug>,
}

enum FlushCommand {
    Item(ScanItemOutcome),
    Barrier(oneshot::Sender<()>),
}

struct BulkInFlightGuard<'a, R: HomeScanRunner> {
    inner: &'a EngineInner<R>,
    batches: Vec<Arc<ScanBatchState>>,
}

impl<'a, R: HomeScanRunner> BulkInFlightGuard<'a, R> {
    fn enter(inner: &'a EngineInner<R>, batches: Vec<Arc<ScanBatchState>>) -> Self {
        inner.in_flight_bulk.fetch_add(1, Ordering::AcqRel);
        for b in &batches {
            b.in_flight.fetch_add(1, Ordering::AcqRel);
        }
        Self { inner, batches }
    }
}

impl<R: HomeScanRunner> Drop for BulkInFlightGuard<'_, R> {
    fn drop(&mut self) {
        self.inner.in_flight_bulk.fetch_sub(1, Ordering::AcqRel);
        for b in &self.batches {
            b.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct InteractiveInFlightGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> InteractiveInFlightGuard<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for InteractiveInFlightGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

struct EngineInner<R: HomeScanRunner> {
    config: ScanEngineConfig,
    runner: R,
    tile_cache: Arc<SpatialTileCache>,
    lazy_hook: std::sync::RwLock<Option<Arc<dyn PeriodicLazyEnqueueHook>>>,
    interactive_sem: Semaphore,
    bulk_sem: Semaphore,
    queue_sem: Semaphore,
    work_queue: Mutex<VecDeque<QueuedWorkItem>>,
    queued_count: AtomicUsize,
    batches: DashMap<String, Arc<ScanBatchState>>,
    inflight_slugs: DashMap<String, Arc<InflightSlug>>,
    flush_tx: mpsc::Sender<FlushCommand>,
    shutdown: AtomicBool,
    active_batches: AtomicUsize,
    in_flight_interactive: AtomicUsize,
    in_flight_bulk: AtomicUsize,
    total_submitted: AtomicU64,
    total_completed: AtomicU64,
    total_deduplicated_slugs: AtomicU64,
    db_flushes_completed: AtomicU64,
    db_rows_flushed: AtomicU64,
}

impl<R: HomeScanRunner> EngineInner<R> {
    async fn execute_queued_item(self: &Arc<Self>, work: QueuedWorkItem) {
        let QueuedWorkItem { task, inflight } = work;

        // Fast 0ms abort path if all waiting batches for this slug were cancelled while queued.
        if inflight.all_subscribers_cancelled() {
            self.finish_inflight(&inflight, None).await;
            return;
        }

        let Ok(_bulk_permit) = self.bulk_sem.acquire().await else {
            self.finish_inflight(&inflight, None).await;
            return;
        };

        if inflight.all_subscribers_cancelled() {
            self.finish_inflight(&inflight, None).await;
            return;
        }

        let active_batches = inflight.mark_slots_running_and_collect_batches();
        let outcome_opt = {
            let _guard = BulkInFlightGuard::enter(self, active_batches);
            let cancel_fut = async {
                loop {
                    let notified = inflight.cancel_notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if inflight.all_subscribers_cancelled() || self.shutdown.load(Ordering::Acquire)
                    {
                        break;
                    }
                    notified.await;
                }
            };

            tokio::select! {
                biased;
                _ = cancel_fut => None,
                res = self.runner.scan_one(task) => Some(res),
            }
        };

        // Release `_bulk_permit` before awaiting any completion flush barriers so workers never stall!
        drop(_bulk_permit);

        self.finish_inflight(&inflight, outcome_opt).await;
    }

    async fn finish_inflight(
        self: &Arc<Self>,
        inflight: &Arc<InflightSlug>,
        outcome_opt: Option<Result<ScanItemOutcome>>,
    ) {
        // 1. Atomically unpublish from `inflight_slugs` (only if still pointing to this flight)
        //    and seal subscriber list so no late batch can attach to a finished flight.
        self.inflight_slugs
            .remove_if(&inflight.slug, |_, v| Arc::ptr_eq(v, inflight));
        let subscribers = inflight.seal_and_take_subscribers();
        if subscribers.is_empty() {
            return;
        }

        let any_live_subscriber = subscribers.iter().any(|s| !s.batch.is_cancelled());

        // 2. Enqueue unique successful scan outcome into Micro-Batch Persistence BEFORE
        //    incrementing batch `resolved_slots`, guaranteeing FIFO ordering ahead of any
        //    batch-completion `FlushCommand::Barrier`.
        if let Some(Ok(ref outcome)) = outcome_opt {
            if any_live_subscriber {
                let _ = self
                    .flush_tx
                    .send(FlushCommand::Item(outcome.clone()))
                    .await;
            }
        }

        // 3. Fan out result to primary + deduplicated subscriber slots across all attached batches.
        let mut batches_ready_to_finalize: Vec<Arc<ScanBatchState>> = Vec::new();

        for sub in subscribers {
            let slot = &sub.batch.items[sub.slot_index];
            if sub.batch.is_cancelled() || outcome_opt.is_none() {
                slot.state_code.store(ITEM_CANCELLED, Ordering::Release);
                sub.batch.cancelled_items.fetch_add(1, Ordering::AcqRel);
            } else {
                match &outcome_opt {
                    Some(Ok(outcome)) => {
                        let is_cached_or_dedup = !sub.is_primary || outcome.cached;
                        slot.cached_flag
                            .store(is_cached_or_dedup, Ordering::Release);
                        if !sub.is_primary {
                            slot.deduplicated_flag.store(true, Ordering::Release);
                        }
                        {
                            let mut det = slot.detail.lock().unwrap_or_else(|e| e.into_inner());
                            det.duration_ms = Some(outcome.duration_ms);
                            det.overall_score = outcome.overall_score;
                            det.estimated_value = outcome.estimated_value;
                        }
                        slot.state_code.store(
                            if is_cached_or_dedup {
                                ITEM_CACHED
                            } else {
                                ITEM_SUCCEEDED
                            },
                            Ordering::Release,
                        );
                        if is_cached_or_dedup {
                            sub.batch.cached.fetch_add(1, Ordering::AcqRel);
                        }
                        sub.batch.succeeded.fetch_add(1, Ordering::AcqRel);
                        sub.batch.completed.fetch_add(1, Ordering::AcqRel);
                        self.total_completed.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(Err(err)) => {
                        {
                            let mut det = slot.detail.lock().unwrap_or_else(|e| e.into_inner());
                            det.error = Some(err.to_string());
                        }
                        slot.state_code.store(ITEM_FAILED, Ordering::Release);
                        sub.batch.failed.fetch_add(1, Ordering::AcqRel);
                        sub.batch.completed.fetch_add(1, Ordering::AcqRel);
                        self.total_completed.fetch_add(1, Ordering::Relaxed);
                    }
                    None => unreachable!(),
                }
            }

            let resolved = sub.batch.resolved_slots.fetch_add(1, Ordering::AcqRel) + 1;
            if resolved == sub.batch.total {
                batches_ready_to_finalize.push(sub.batch);
            }
        }

        // 4. Finalize any batch whose final slot just resolved (including flushing tail DB batch).
        for batch in batches_ready_to_finalize {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                this.finalize_batch(batch).await;
            });
        }
    }

    async fn finalize_batch(&self, batch: Arc<ScanBatchState>) {
        // Release the `attached_inflights` references promptly to free memory.
        {
            let mut attached = batch
                .attached_inflights
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            attached.clear();
            attached.shrink_to_fit();
        }

        if !batch.is_cancelled() {
            // Ensure every row belonging to this batch has been committed by the micro-batch writer
            // before flipping batch status to "completed".
            let (tx, rx) = oneshot::channel();
            if self.flush_tx.send(FlushCommand::Barrier(tx)).await.is_ok() {
                let _ = rx.await;
            }
            let _ = batch.status_code.compare_exchange(
                STATUS_RUNNING,
                STATUS_COMPLETED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }

        let now_ms = now_unix_ms() as u64;
        let _ =
            batch
                .finished_at_ms
                .compare_exchange(0, now_ms, Ordering::AcqRel, Ordering::Relaxed);
        if batch.counted_active.swap(false, Ordering::AcqRel) {
            self.active_batches.fetch_sub(1, Ordering::AcqRel);
        }
        batch.done_notify.notify_waiters();
    }

    async fn run_flush_loop(self: Arc<Self>, mut rx: mpsc::Receiver<FlushCommand>) {
        let batch_size = self.config.db_flush_batch_size.max(1);
        let interval = Duration::from_millis(self.config.db_flush_interval_ms.max(5));
        let mut buffer: Vec<ScanItemOutcome> = Vec::with_capacity(batch_size);
        let mut deadline: Option<Instant> = None;

        loop {
            if buffer.is_empty() {
                deadline = None;
                let Some(cmd) = rx.recv().await else {
                    break;
                };
                match cmd {
                    FlushCommand::Item(item) => {
                        buffer.push(item);
                        deadline = Some(Instant::now() + interval);
                        // Greedily drain ready items up to `batch_size` without blocking.
                        while buffer.len() < batch_size {
                            match rx.try_recv() {
                                Ok(FlushCommand::Item(next)) => buffer.push(next),
                                Ok(FlushCommand::Barrier(tx)) => {
                                    self.flush_buffer(&mut buffer).await;
                                    deadline = None;
                                    let _ = tx.send(());
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if buffer.len() >= batch_size {
                            self.flush_buffer(&mut buffer).await;
                            deadline = None;
                        }
                    }
                    FlushCommand::Barrier(tx) => {
                        let _ = tx.send(());
                    }
                }
            } else if buffer.len() >= batch_size {
                self.flush_buffer(&mut buffer).await;
                deadline = None;
            } else {
                let sleep_until = deadline.unwrap_or_else(|| Instant::now() + interval);
                tokio::select! {
                    biased;
                    maybe_cmd = rx.recv() => {
                        match maybe_cmd {
                            Some(FlushCommand::Item(item)) => {
                                buffer.push(item);
                                if buffer.len() >= batch_size {
                                    self.flush_buffer(&mut buffer).await;
                                    deadline = None;
                                }
                            }
                            Some(FlushCommand::Barrier(tx)) => {
                                self.flush_buffer(&mut buffer).await;
                                deadline = None;
                                let _ = tx.send(());
                            }
                            None => {
                                self.flush_buffer(&mut buffer).await;
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(sleep_until) => {
                        self.flush_buffer(&mut buffer).await;
                        deadline = None;
                    }
                }
            }
        }
    }

    async fn flush_buffer(&self, buffer: &mut Vec<ScanItemOutcome>) {
        if buffer.is_empty() {
            return;
        }
        let count = buffer.len() as u64;
        let chunk = std::mem::replace(
            buffer,
            Vec::with_capacity(self.config.db_flush_batch_size.max(1)),
        );
        let hook = self
            .lazy_hook
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let hook_chunk = hook.as_ref().map(|_| chunk.clone());
        if let Err(err) = self.runner.flush_batch(chunk).await {
            tracing::warn!(
                "ScanEngine micro-batch flush error ({} rows): {}",
                count,
                err
            );
        } else {
            self.db_flushes_completed.fetch_add(1, Ordering::Relaxed);
            self.db_rows_flushed.fetch_add(count, Ordering::Relaxed);
            if let (Some(h), Some(outcomes)) = (hook, hook_chunk) {
                h.on_batch_flushed(&outcomes).await;
            }
        }
    }
}

struct ShutdownGuard<R: HomeScanRunner> {
    inner: Arc<EngineInner<R>>,
}

impl<R: HomeScanRunner> Drop for ShutdownGuard<R> {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.queue_sem.close();
        self.inner.bulk_sem.close();
    }
}

#[derive(Clone)]
pub struct ScanEngine<R: HomeScanRunner = DynScanRunner> {
    inner: Arc<EngineInner<R>>,
    _shutdown_guard: Arc<ShutdownGuard<R>>,
}

impl<R: HomeScanRunner> ScanEngine<R> {
    pub fn new(config: ScanEngineConfig, tile_cache: Arc<SpatialTileCache>, runner: R) -> Self {
        let (interactive_permits, bulk_permits) = config.effective_lane_permits();
        let flush_cap = config.max_queued_homes.clamp(4_096, 65_536);
        let (flush_tx, flush_rx) = mpsc::channel(flush_cap);

        let inflight_cap = config.max_queued_homes.clamp(1_024, 32_768);
        let inner = Arc::new(EngineInner {
            work_queue: Mutex::new(VecDeque::with_capacity(config.max_queued_homes.min(16_384))),
            config,
            runner,
            tile_cache,
            lazy_hook: std::sync::RwLock::new(None),
            interactive_sem: Semaphore::new(interactive_permits),
            bulk_sem: Semaphore::new(bulk_permits),
            queue_sem: Semaphore::new(0),
            queued_count: AtomicUsize::new(0),
            batches: DashMap::with_capacity(256),
            inflight_slugs: DashMap::with_capacity(inflight_cap),
            flush_tx,
            shutdown: AtomicBool::new(false),
            active_batches: AtomicUsize::new(0),
            in_flight_interactive: AtomicUsize::new(0),
            in_flight_bulk: AtomicUsize::new(0),
            total_submitted: AtomicU64::new(0),
            total_completed: AtomicU64::new(0),
            total_deduplicated_slugs: AtomicU64::new(0),
            db_flushes_completed: AtomicU64::new(0),
            db_rows_flushed: AtomicU64::new(0),
        });

        // Spawn dedicated Micro-Batch Persistence Task
        {
            let flusher_inner = Arc::clone(&inner);
            tokio::spawn(async move {
                flusher_inner.run_flush_loop(flush_rx).await;
            });
        }

        // Spawn bounded P1 Bulk Worker Pool (`bulk_permits` persistent workers)
        for _ in 0..bulk_permits {
            let worker_inner = Arc::clone(&inner);
            tokio::spawn(async move {
                loop {
                    let Ok(permit) = worker_inner.queue_sem.acquire().await else {
                        break;
                    };
                    permit.forget();

                    let maybe_work = {
                        let mut q = worker_inner
                            .work_queue
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let popped = q.pop_front();
                        if popped.is_some() {
                            worker_inner.queued_count.fetch_sub(1, Ordering::Release);
                        }
                        popped
                    };

                    if let Some(work) = maybe_work {
                        worker_inner.execute_queued_item(work).await;
                    }
                }
            });
        }

        let guard = Arc::new(ShutdownGuard {
            inner: Arc::clone(&inner),
        });
        Self {
            inner,
            _shutdown_guard: guard,
        }
    }

    pub fn with_runner(config: ScanEngineConfig, runner: R) -> Self {
        Self::new(config, Arc::new(SpatialTileCache::default()), runner)
    }

    pub fn set_lazy_hook(&self, hook: Option<Arc<dyn PeriodicLazyEnqueueHook>>) {
        let mut guard = self
            .inner
            .lazy_hook
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *guard = hook;
    }

    pub fn with_lazy_hook(self, hook: Arc<dyn PeriodicLazyEnqueueHook>) -> Self {
        self.set_lazy_hook(Some(hook));
        self
    }

    pub fn tile_cache(&self) -> &Arc<SpatialTileCache> {
        &self.inner.tile_cache
    }

    pub fn config(&self) -> &ScanEngineConfig {
        &self.inner.config
    }

    /// Enqueue up to `max_batch_submission` (default 20,000) homes in a single non-blocking call.
    /// Deduplicates identical canonical slugs within the batch AND across currently in-flight
    /// slugs (`inflight_slugs`), pushing unique work into the bounded P1 bulk worker pool in `<20ms`.
    pub fn enqueue_batch(&self, req: BatchScanRequest) -> Result<BatchScanAccepted> {
        let mut normalized_tasks: Vec<ScanTask> =
            Vec::with_capacity(req.tasks.len() + req.addresses.len());

        for mut t in req.tasks {
            if t.address.trim().is_empty() && t.slug.trim().is_empty() {
                continue;
            }
            if req.force_refresh {
                t.force_refresh = true;
            }
            if req.skip_ai {
                t.skip_ai = true;
            }
            t.priority = ScanPriority::Bulk;
            t.ensure_canonical_slug();
            normalized_tasks.push(t);
        }

        for raw_addr in req.addresses {
            let trimmed = collapse_addr_ws(&raw_addr);
            if trimmed.is_empty() {
                continue;
            }
            let mut t = ScanTask::new(trimmed);
            t.force_refresh = req.force_refresh;
            t.skip_ai = req.skip_ai;
            t.priority = ScanPriority::Bulk;
            normalized_tasks.push(t);
        }

        let total_submitted = normalized_tasks.len();
        if total_submitted == 0 {
            return Err(anyhow!(
                "BatchScanRequest must contain at least 1 valid address"
            ));
        }
        if total_submitted > self.inner.config.max_batch_submission {
            return Err(anyhow!(
                "Batch submission of {} homes exceeds max_batch_submission ({})",
                total_submitted,
                self.inner.config.max_batch_submission
            ));
        }

        // Group slots by canonical slug within this batch (preserving input slot indices).
        let mut slug_groups: HashMap<String, (ScanTask, Vec<usize>)> =
            HashMap::with_capacity(total_submitted);
        let mut unique_slug_order: Vec<String> = Vec::with_capacity(total_submitted);
        let mut item_slots: Vec<BatchItemSlot> = Vec::with_capacity(total_submitted);
        let mut deduplicated_within_batch = 0usize;

        for (idx, task) in normalized_tasks.into_iter().enumerate() {
            let slug = task.slug.clone();
            let address = task.address.clone();
            let is_dup_within = if let Some((_, indices)) = slug_groups.get_mut(&slug) {
                indices.push(idx);
                deduplicated_within_batch += 1;
                true
            } else {
                unique_slug_order.push(slug.clone());
                slug_groups.insert(slug.clone(), (task, vec![idx]));
                false
            };

            item_slots.push(BatchItemSlot {
                slug,
                address,
                deduplicated_initial: is_dup_within,
                state_code: AtomicU8::new(ITEM_QUEUED),
                cached_flag: AtomicBool::new(is_dup_within),
                deduplicated_flag: AtomicBool::new(is_dup_within),
                detail: Mutex::new(ItemSlotDetail::default()),
            });
        }

        // Pre-check queue capacity before mutating `inflight_slugs`.
        let max_needed_queue_slots = unique_slug_order.len();
        if self.inner.queued_count.load(Ordering::Acquire) + max_needed_queue_slots
            > self.inner.config.max_queued_homes
        {
            return Err(anyhow!(
                "ScanEngine queue backpressure: {} queued + {} new exceeds max_queued_homes ({})",
                self.inner.queued_count.load(Ordering::Relaxed),
                max_needed_queue_slots,
                self.inner.config.max_queued_homes
            ));
        }

        let batch_id = format!("batch_{}", Uuid::new_v4().simple());
        let started_at_ms = now_unix_ms() as u64;

        // Temporary placeholder `unique_queued`; we know exact `unique_queued` after checking cross-batch `inflight_slugs`.
        let batch_state = Arc::new(ScanBatchState {
            batch_id: batch_id.clone(),
            label: req.label,
            total: total_submitted,
            unique_queued: AtomicUsize::new(unique_slug_order.len()),
            started_at_ms,
            finished_at_ms: AtomicU64::new(0),
            status_code: AtomicU8::new(STATUS_RUNNING),
            cancelled: AtomicBool::new(false),
            counted_active: AtomicBool::new(true),
            completed: AtomicUsize::new(0),
            cached: AtomicUsize::new(0),
            succeeded: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            cancelled_items: AtomicUsize::new(0),
            resolved_slots: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            items: item_slots,
            attached_inflights: Mutex::new(Vec::with_capacity(unique_slug_order.len())),
            done_notify: Notify::new(),
        });

        const MAX_RETAINED_TERMINAL_BATCHES: usize = 512;
        if self.inner.batches.len() > MAX_RETAINED_TERMINAL_BATCHES {
            let mut terminal: Vec<(String, u64)> = self
                .inner
                .batches
                .iter()
                .filter_map(|e| {
                    let st = e.value();
                    if st.is_terminal() {
                        Some((e.key().clone(), st.finished_at_ms.load(Ordering::Relaxed)))
                    } else {
                        None
                    }
                })
                .collect();
            if terminal.len() > MAX_RETAINED_TERMINAL_BATCHES {
                terminal.sort_by_key(|(_, ts)| *ts);
                let evict_count = terminal.len() - MAX_RETAINED_TERMINAL_BATCHES;
                for (old_id, _) in terminal.into_iter().take(evict_count) {
                    self.inner.batches.remove(&old_id);
                }
            }
        }

        self.inner.active_batches.fetch_add(1, Ordering::AcqRel);
        self.inner
            .batches
            .insert(batch_id.clone(), Arc::clone(&batch_state));

        let mut work_to_enqueue: Vec<QueuedWorkItem> = Vec::with_capacity(unique_slug_order.len());
        let mut attached_refs: Vec<Arc<InflightSlug>> = Vec::with_capacity(unique_slug_order.len());
        let mut deduplicated_inflight = 0usize;

        for slug in unique_slug_order {
            let Some((task, slot_indices)) = slug_groups.remove(&slug) else {
                continue;
            };

            match self.inner.inflight_slugs.entry(slug.clone()) {
                Entry::Occupied(mut occ) => {
                    let existing = Arc::clone(occ.get());
                    let attached_to_existing = {
                        let mut st = existing.state.lock().unwrap_or_else(|e| e.into_inner());
                        if !st.sealed {
                            for &slot_idx in &slot_indices {
                                batch_state.items[slot_idx]
                                    .deduplicated_flag
                                    .store(true, Ordering::Release);
                                batch_state.items[slot_idx]
                                    .cached_flag
                                    .store(true, Ordering::Release);
                                st.subscribers.push(SubscriberSlot {
                                    batch: Arc::clone(&batch_state),
                                    slot_index: slot_idx,
                                    is_primary: false,
                                });
                            }
                            true
                        } else {
                            false
                        }
                    };

                    if attached_to_existing {
                        deduplicated_inflight += 1;
                        attached_refs.push(existing);
                    } else {
                        let subs = build_primary_and_dup_subs(&batch_state, &slot_indices);
                        let fresh = InflightSlug::new(slug, subs);
                        occ.insert(Arc::clone(&fresh));
                        attached_refs.push(Arc::clone(&fresh));
                        work_to_enqueue.push(QueuedWorkItem {
                            task,
                            inflight: fresh,
                        });
                    }
                }
                Entry::Vacant(vac) => {
                    let subs = build_primary_and_dup_subs(&batch_state, &slot_indices);
                    let fresh = InflightSlug::new(slug, subs);
                    vac.insert(Arc::clone(&fresh));
                    attached_refs.push(Arc::clone(&fresh));
                    work_to_enqueue.push(QueuedWorkItem {
                        task,
                        inflight: fresh,
                    });
                }
            }
        }

        let unique_queued = work_to_enqueue.len();
        batch_state
            .unique_queued
            .store(unique_queued, Ordering::Release);

        if !batch_state.is_terminal() {
            let mut att = batch_state
                .attached_inflights
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !batch_state.is_terminal() {
                *att = attached_refs;
            }
        }
        self.inner
            .total_submitted
            .fetch_add(total_submitted as u64, Ordering::Relaxed);
        self.inner.total_deduplicated_slugs.fetch_add(
            (deduplicated_within_batch + deduplicated_inflight) as u64,
            Ordering::Relaxed,
        );

        if unique_queued > 0 {
            {
                let mut q = self
                    .inner
                    .work_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                self.inner
                    .queued_count
                    .fetch_add(unique_queued, Ordering::Release);
                q.extend(work_to_enqueue);
            }
            // Single atomic wakeup for up to `unique_queued` waiting worker tasks!
            self.inner.queue_sem.add_permits(unique_queued);
        }

        Ok(BatchScanAccepted {
            ok: true,
            batch_id,
            total_submitted,
            unique_queued,
            deduplicated_within_batch,
            deduplicated_inflight,
            status: "running".to_string(),
        })
    }

    /// Execute a single-home scan immediately on the dedicated P0 Interactive Lane
    /// (`interactive_reserved_permits`), completely bypassing the P1 bulk queue!
    pub async fn run_interactive(&self, mut task: ScanTask) -> Result<ScanItemOutcome> {
        task.priority = ScanPriority::Interactive;
        task.ensure_canonical_slug();

        let _permit = self
            .inner
            .interactive_sem
            .acquire()
            .await
            .map_err(|_| anyhow!("Interactive scanner semaphore closed"))?;

        let _guard = InteractiveInFlightGuard::enter(&self.inner.in_flight_interactive);
        self.inner.total_submitted.fetch_add(1, Ordering::Relaxed);

        let outcome = self.inner.runner.scan_one(task).await?;
        self.inner.total_completed.fetch_add(1, Ordering::Relaxed);

        // Persist asynchronously via the Micro-Batch Persistence Task without delaying P0 caller.
        let outcome_for_db = outcome.clone();
        if let Err(mpsc::error::TrySendError::Full(cmd)) = self
            .inner
            .flush_tx
            .try_send(FlushCommand::Item(outcome_for_db))
        {
            let tx = self.inner.flush_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(cmd).await;
            });
        }

        Ok(outcome)
    }

    /// Lock-free atomic progress & paginated item slice for a batch job.
    pub fn get_batch_status(
        &self,
        batch_id: &str,
        offset: usize,
        limit: usize,
    ) -> Option<BatchScanStatus> {
        let batch = self.inner.batches.get(batch_id)?.value().clone();

        let status = batch.status_str().to_string();
        let total = batch.total;
        let completed = batch.completed.load(Ordering::Acquire);
        let cached = batch.cached.load(Ordering::Acquire);
        let succeeded = batch.succeeded.load(Ordering::Acquire);
        let failed = batch.failed.load(Ordering::Acquire);
        let cancelled = batch.cancelled_items.load(Ordering::Acquire);
        let in_flight = if batch.is_cancelled() {
            0
        } else {
            batch.in_flight.load(Ordering::Acquire)
        };

        let now_ms = now_unix_ms() as u64;
        let end_ms = {
            let fin = batch.finished_at_ms.load(Ordering::Acquire);
            if fin > 0 {
                fin
            } else {
                now_ms
            }
        };
        let elapsed_ms = end_ms.saturating_sub(batch.started_at_ms);

        let throughput_homes_per_sec = if completed > 0 {
            let secs = (elapsed_ms.max(1) as f64) / 1000.0;
            round_n(completed as f64 / secs, 2)
        } else {
            0.0
        };

        let eta_sec = match status.as_str() {
            "completed" => Some(0.0),
            "cancelled" => None,
            _ => {
                let remaining = total.saturating_sub(completed + cancelled);
                if remaining == 0 {
                    Some(0.0)
                } else if throughput_homes_per_sec > 0.0 {
                    Some(round_n(remaining as f64 / throughput_homes_per_sec, 2))
                } else {
                    None
                }
            }
        };

        let safe_offset = offset.min(total);
        let safe_end = safe_offset.saturating_add(limit).min(total);
        let mut items = Vec::with_capacity(safe_end.saturating_sub(safe_offset));

        let batch_cancelled = batch.is_cancelled();
        for (idx, slot) in batch.items[safe_offset..safe_end]
            .iter()
            .enumerate()
            .map(|(rel, s)| (safe_offset + rel, s))
        {
            let raw_code = slot.state_code.load(Ordering::Acquire);
            let effective_code =
                if batch_cancelled && (raw_code == ITEM_QUEUED || raw_code == ITEM_RUNNING) {
                    ITEM_CANCELLED
                } else {
                    raw_code
                };
            let item_status = match effective_code {
                ITEM_RUNNING => "running",
                ITEM_SUCCEEDED => "succeeded",
                ITEM_CACHED => "cached",
                ITEM_FAILED => "failed",
                ITEM_CANCELLED => "cancelled",
                _ => "queued",
            };
            let det = slot.detail.lock().unwrap_or_else(|e| e.into_inner());
            items.push(BatchScanItemStatus {
                index: idx,
                slug: slot.slug.clone(),
                address: slot.address.clone(),
                status: item_status.to_string(),
                cached: slot.cached_flag.load(Ordering::Acquire),
                deduplicated: slot.deduplicated_flag.load(Ordering::Acquire)
                    || slot.deduplicated_initial,
                duration_ms: det.duration_ms,
                overall_score: det.overall_score,
                estimated_value: det.estimated_value,
                error: det.error.clone(),
            });
        }

        Some(BatchScanStatus {
            batch_id: batch.batch_id.clone(),
            status,
            total,
            unique_queued: batch.unique_queued.load(Ordering::Acquire),
            completed,
            cached,
            succeeded,
            failed,
            cancelled,
            in_flight,
            elapsed_ms,
            throughput_homes_per_sec,
            eta_sec,
            offset: safe_offset,
            limit,
            items,
        })
    }

    /// Atomically cancel a running batch job so queued items abort in 0ms and any single-owner
    /// in-flight scans abort immediately without wasting upstream API quota.
    pub fn cancel_batch(&self, batch_id: &str) -> bool {
        let Some(entry) = self.inner.batches.get(batch_id) else {
            return false;
        };
        let batch = Arc::clone(entry.value());
        drop(entry);

        if batch
            .status_code
            .compare_exchange(
                STATUS_RUNNING,
                STATUS_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return false;
        }

        batch.cancelled.store(true, Ordering::SeqCst);
        let now_ms = now_unix_ms() as u64;
        let _ =
            batch
                .finished_at_ms
                .compare_exchange(0, now_ms, Ordering::AcqRel, Ordering::Relaxed);
        if batch.counted_active.swap(false, Ordering::AcqRel) {
            self.inner.active_batches.fetch_sub(1, Ordering::AcqRel);
        }

        let inflights = {
            let mut guard = batch
                .attached_inflights
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        for inf in inflights {
            inf.cancel_notify.notify_waiters();
        }

        batch.done_notify.notify_waiters();
        true
    }

    /// Await terminal completion (`"completed"` or `"cancelled"`) of a batch without busy-polling.
    pub async fn wait_for_batch(
        &self,
        batch_id: &str,
        offset: usize,
        limit: usize,
    ) -> Option<BatchScanStatus> {
        let batch = self.inner.batches.get(batch_id)?.value().clone();
        loop {
            let notified = batch.done_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if batch.is_terminal() {
                break;
            }
            notified.await;
        }
        self.get_batch_status(batch_id, offset, limit)
    }

    /// Force an immediate micro-batch persistence barrier flush and await completion.
    pub async fn flush_now(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .flush_tx
            .send(FlushCommand::Barrier(tx))
            .await
            .map_err(|_| anyhow!("Micro-batch flusher channel closed"))?;
        rx.await
            .map_err(|_| anyhow!("Micro-batch flusher barrier dropped"))
    }

    /// Live telemetry snapshot for `/api/health` and `/api/scan/metrics`.
    pub fn metrics_json(&self) -> Value {
        let tile_stats = self.inner.tile_cache.stats();
        let total_sub = self.inner.total_submitted.load(Ordering::Relaxed);
        let total_dedup = self.inner.total_deduplicated_slugs.load(Ordering::Relaxed);
        let slug_dedup_ratio = if total_sub > 0 {
            total_dedup as f64 / total_sub as f64
        } else {
            0.0
        };
        let dedup_ratio = if tile_stats.hits + tile_stats.misses > 0 {
            tile_stats.deduplication_ratio
        } else {
            slug_dedup_ratio
        };

        json!({
            "activeBatches": self.inner.active_batches.load(Ordering::Relaxed),
            "totalBatches": self.inner.batches.len(),
            "queuedHomes": self.inner.queued_count.load(Ordering::Relaxed),
            "maxQueuedHomes": self.inner.config.max_queued_homes,
            "maxConcurrentScans": self.inner.config.max_concurrent_scans,
            "interactiveReservedPermits": self.inner.config.interactive_reserved_permits,
            "inFlightInteractive": self.inner.in_flight_interactive.load(Ordering::Relaxed),
            "inFlightBulk": self.inner.in_flight_bulk.load(Ordering::Relaxed),
            "totalSubmitted": total_sub,
            "totalCompleted": self.inner.total_completed.load(Ordering::Relaxed),
            "totalDeduplicatedSlugs": total_dedup,
            "dbFlushesCompleted": self.inner.db_flushes_completed.load(Ordering::Relaxed),
            "dbRowsFlushed": self.inner.db_rows_flushed.load(Ordering::Relaxed),
            "tileCacheHits": tile_stats.hits,
            "tileCacheUpstreamCalls": tile_stats.misses,
            "tileCacheCoalescedWaiters": tile_stats.coalesced_waiters,
            "deduplicationRatio": round_n(dedup_ratio, 6),
            "slugDeduplicationRatio": round_n(slug_dedup_ratio, 6),
        })
    }
}

impl ScanEngine<DynScanRunner> {
    pub fn new_dyn<T: HomeScanRunner>(
        config: ScanEngineConfig,
        tile_cache: Arc<SpatialTileCache>,
        runner: T,
    ) -> Self {
        ScanEngine::new(config, tile_cache, DynScanRunner::new(runner))
    }

    pub fn from_closures<ScanFn, ScanFut, FlushFn, FlushFut>(
        config: ScanEngineConfig,
        tile_cache: Arc<SpatialTileCache>,
        scan_fn: ScanFn,
        flush_fn: FlushFn,
    ) -> Self
    where
        ScanFn: Fn(ScanTask) -> ScanFut + Send + Sync + 'static,
        ScanFut: Future<Output = Result<ScanItemOutcome>> + Send + 'static,
        FlushFn: Fn(Vec<ScanItemOutcome>) -> FlushFut + Send + Sync + 'static,
        FlushFut: Future<Output = Result<()>> + Send + 'static,
    {
        ScanEngine::new_dyn(config, tile_cache, ClosureRunner { scan_fn, flush_fn })
    }
}

fn build_primary_and_dup_subs(
    batch: &Arc<ScanBatchState>,
    slot_indices: &[usize],
) -> Vec<SubscriberSlot> {
    slot_indices
        .iter()
        .enumerate()
        .map(|(pos, &slot_idx)| SubscriberSlot {
            batch: Arc::clone(batch),
            slot_index: slot_idx,
            is_primary: pos == 0,
        })
        .collect()
}

fn is_clean_whitespace(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty()
        || bytes[0].is_ascii_whitespace()
        || bytes[bytes.len() - 1].is_ascii_whitespace()
    {
        return false;
    }
    let mut prev_space = false;
    for &b in bytes {
        if b.is_ascii_whitespace() {
            if b != b' ' || prev_space {
                return false;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
    }
    true
}

fn is_already_canonical_slug(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 80 || bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    let mut prev_dash = false;
    for &b in bytes {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => prev_dash = false,
            b'-' if !prev_dash => prev_dash = true,
            _ => return false,
        }
    }
    true
}

fn collapse_addr_ws(s: &str) -> String {
    if is_clean_whitespace(s) {
        return s.to_string();
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn canonical_address_slug(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_ascii() && !trimmed.is_empty() {
        let mut out = String::with_capacity(trimmed.len().min(80));
        let mut prev_dash = true; // suppresses leading '-'
        for b in trimmed.bytes() {
            if out.len() >= 80 {
                break;
            }
            match b {
                b'a'..=b'z' | b'0'..=b'9' => {
                    out.push(b as char);
                    prev_dash = false;
                }
                b'A'..=b'Z' => {
                    out.push((b + 32) as char);
                    prev_dash = false;
                }
                _ => {
                    if !prev_dash {
                        out.push('-');
                        prev_dash = true;
                    }
                }
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        if !out.is_empty() {
            return out;
        }
    }

    let s = slugify(trimmed);
    if !s.is_empty() {
        s
    } else {
        let mut h = Sha256::new();
        h.update(raw.as_bytes());
        format!("addr-{}", &hex::encode(h.finalize())[..16])
    }
}
