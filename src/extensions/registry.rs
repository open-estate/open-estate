//! Thread-safe runtime `ExtensionHub` and hot-swappable `RuntimeRegistrySnapshot`.
//!
//! Loads all built-in declarative YAML & OpenAPI 3.x integration packages (`extensions/*/manifest.yaml`),
//! supports zero-restart admin mutations (`enable`, `set_priority`, `install_yaml`, `uninstall`,
//! `set_module_enabled`), executes live `test_fetch` probes returning binary Protobuf wire bytes +
//! canonical JSON + latency, and synchronizes cleanly with SQLite / MySQL `installed_extensions` and
//! `system_modules`.

use crate::db::{DbStore, InstalledExtensionRow, SystemModuleRow, DEFAULT_SYSTEM_MODULES};
use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::aggregator::aggregate_envelopes;
use crate::extensions::manifest::{parse_and_validate_package, ParsedExtensionPackage};
use crate::extensions::proto::{
    capability_domain_slug, driver_kind_label, driver_kind_slug, encode_proto_base64,
    encode_proto_bytes, envelope_to_display_json, manifest_execution_mode, manifest_is_lazy,
    manifest_license_class, manifest_metering_dimensions, manifest_min_plan_tier,
    manifest_section_id, manifest_tier, manifest_to_active_llm_spec, provenance_tier_slug,
    AggregatedPropertyBundleProto, CapabilityDomain, DriverKind, ExtensionEnvelopeProto,
    ExtensionManifestProto, ExtensionQueryProto, ProvenanceTier,
};
use crate::extensions::yaml_runner::{
    build_extension_rate_limiter, execute_catalog_rules, execute_live_http,
    execute_upstream_yaml_mapper, is_optional_auth_missing, preview_http_resolution,
    CachedEnvelopeEntry,
};
use crate::lazy::{enqueue as enqueue_lazy_job, EnqueueLazyJobRequest, LazySectionStatusKind};
use crate::metering::{
    default_oss_authorize, mark_cached, mark_denied, mark_failed, with_active_llm_spec,
    ExtensionTier, ExtensionUsageEvent, InvocationContext, InvocationDecision, InvocationTrigger,
    MeterScope,
};
use crate::shared::{now_iso, EnvConfig, HttpClient};
use anyhow::{anyhow, Context, Result};
use futures::future::join_all;
use governor::DefaultDirectRateLimiter;
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LazyEnqueueSummary {
    pub targets_examined: usize,
    pub jobs_evaluated: usize,
    pub queued: usize,
    pub deduplicated: usize,
    pub awaiting_approval: usize,
    pub not_enabled: usize,
    pub errors: usize,
}

pub type PeriodicLazyEnqueueSummary = LazyEnqueueSummary;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, Instant};

tokio::task_local! {
    static ACTIVE_HOOK_STATE: crate::web::server::AppState;
}

/// Callback reconstructing an `AppState` bound to `&Arc<ExtensionHub>` without creating an `Arc` cycle.
pub type HookStateBuilder = Arc<dyn Fn(&Arc<ExtensionHub>) -> crate::web::server::AppState + Send + Sync>;

struct EmbeddedBuiltinSpec {
    #[allow(dead_code)]
    id: &'static str,
    manifest_yaml: &'static str,
    openapi_yaml: Option<&'static str>,
}

const EMBEDDED_BUILTINS: &[EmbeddedBuiltinSpec] = &[
    EmbeddedBuiltinSpec {
        id: "census_geocoder",
        manifest_yaml: include_str!("../../extensions/census_geocoder/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/census_geocoder/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "ip_geolocation",
        manifest_yaml: include_str!("../../extensions/ip_geolocation/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/ip_geolocation/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "fcc_broadband_bdc",
        manifest_yaml: include_str!("../../extensions/fcc_broadband_bdc/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/fcc_broadband_bdc/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "fema_nfhl",
        manifest_yaml: include_str!("../../extensions/fema_nfhl/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/fema_nfhl/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "usgs_asce7_seismic",
        manifest_yaml: include_str!("../../extensions/usgs_asce7_seismic/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/usgs_asce7_seismic/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "epa_sdwis_water",
        manifest_yaml: include_str!("../../extensions/epa_sdwis_water/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/epa_sdwis_water/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "us_municipal_electric",
        manifest_yaml: include_str!("../../extensions/us_municipal_electric/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "us_deregulated_electric",
        manifest_yaml: include_str!("../../extensions/us_deregulated_electric/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "us_iou_electric",
        manifest_yaml: include_str!("../../extensions/us_iou_electric/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "us_natural_gas",
        manifest_yaml: include_str!("../../extensions/us_natural_gas/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "us_water_sewer_trash",
        manifest_yaml: include_str!("../../extensions/us_water_sewer_trash/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "us_fiber_carriers",
        manifest_yaml: include_str!("../../extensions/us_fiber_carriers/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "us_cable_wireless_isp",
        manifest_yaml: include_str!("../../extensions/us_cable_wireless_isp/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "county_recorder_liens",
        manifest_yaml: include_str!("../../extensions/county_recorder_liens/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "preforeclosure_distress",
        manifest_yaml: include_str!("../../extensions/preforeclosure_distress/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "court_litigation_dockets",
        manifest_yaml: include_str!("../../extensions/court_litigation_dockets/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "title_underwriter_rates",
        manifest_yaml: include_str!("../../extensions/title_underwriter_rates/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "fcc_mobile_5g",
        manifest_yaml: include_str!("../../extensions/fcc_mobile_5g/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "planning_rezoning_radar",
        manifest_yaml: include_str!("../../extensions/planning_rezoning_radar/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "osrm_routing",
        manifest_yaml: include_str!("../../extensions/osrm_routing/manifest.yaml"),
        openapi_yaml: Some(include_str!("../../extensions/osrm_routing/openapi.yaml")),
    },
    EmbeddedBuiltinSpec {
        id: "campbell_mgo_permits",
        manifest_yaml: include_str!("../../extensions/campbell_mgo_permits/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "municipal_open_data_permits",
        manifest_yaml: include_str!("../../extensions/municipal_open_data_permits/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "permit_roof_extractor",
        manifest_yaml: include_str!("../../extensions/permit_roof_extractor/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "permit_solar_extractor",
        manifest_yaml: include_str!("../../extensions/permit_solar_extractor/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "permit_mep_extractor",
        manifest_yaml: include_str!("../../extensions/permit_mep_extractor/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "permit_remodel_extractor",
        manifest_yaml: include_str!("../../extensions/permit_remodel_extractor/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "permit_code_enforcement_extractor",
        manifest_yaml: include_str!(
            "../../extensions/permit_code_enforcement_extractor/manifest.yaml"
        ),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "fcc_broadband_bdc_live",
        manifest_yaml: include_str!("../../extensions/fcc_broadband_bdc_live/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "cec_electric_territory",
        manifest_yaml: include_str!("../../extensions/cec_electric_territory/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "cec_gas_territory",
        manifest_yaml: include_str!("../../extensions/cec_gas_territory/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "eia861_electric_rates",
        manifest_yaml: include_str!("../../extensions/eia861_electric_rates/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "openei_urdb_tariffs",
        manifest_yaml: include_str!("../../extensions/openei_urdb_tariffs/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "pge_live_tariffs",
        manifest_yaml: include_str!("../../extensions/pge_live_tariffs/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "sjwater_live_tariffs",
        manifest_yaml: include_str!("../../extensions/sjwater_live_tariffs/manifest.yaml"),
        openapi_yaml: None,
    },
    EmbeddedBuiltinSpec {
        id: "scc_sanitation_district",
        manifest_yaml: include_str!("../../extensions/scc_sanitation_district/manifest.yaml"),
        openapi_yaml: None,
    },
];

enum ExtensionDriverSlot {
    DeclarativeOrOpenApi(Arc<ParsedExtensionPackage>),
    Native(Arc<dyn DataExtensionAdapter>),
}

#[derive(Clone)]
struct RegisteredExtensionEntry {
    extension_id: String,
    manifest: ExtensionManifestProto,
    driver_slot: Arc<ExtensionDriverSlot>,
    manifest_yaml: Option<String>,
    openapi_yaml: Option<String>,
    config_json: String,
    health_status: String,
    last_checked_at: Option<String>,
    installed_at: String,
    updated_at: String,
}

/// Immutable point-in-time snapshot of active `system_modules` and `installed_extensions`
/// held by `AppState` / `orchestrator` / `render` for lock-free per-request gating and cache invalidation.
#[derive(Debug, Clone)]
pub struct RuntimeRegistrySnapshot {
    pub generation_version: u64,
    pub modules: HashMap<String, SystemModuleRow>,
    pub enabled_extension_ids_by_domain: HashMap<i32, Vec<String>>,
    pub manifests: Vec<ExtensionManifestProto>,
}

impl RuntimeRegistrySnapshot {
    /// Returns whether a core `system_module` (e.g. `"utilities_broadband"`, `"natural_hazards"`,
    /// `"ai_analyst"`, `"house_3d_lidar"`, `"ip_geolocation"`) is currently enabled.
    /// Note: `"geocode"` is required core and always remains enabled.
    pub fn is_module_enabled(&self, module_id: &str) -> bool {
        let clean = module_id.trim();
        if clean == "geocode" {
            return true;
        }
        self.modules
            .get(clean)
            .map(|m| m.enabled)
            .unwrap_or(true)
    }

    /// Convenience alias matching `snap.is_enabled(...)` in orchestrator/server call sites.
    pub fn is_enabled(&self, module_id: &str) -> bool {
        self.is_module_enabled(module_id)
    }

    /// Sorted list of all currently enabled `module_id`s for `window.__REPORT__.enabledModules`.
    pub fn enabled_module_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .modules
            .values()
            .filter(|m| m.enabled || m.module_id == "geocode")
            .map(|m| m.module_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Returns whether a specific `extension_id` (e.g. `"fcc_broadband_bdc"`) is currently enabled.
    pub fn is_extension_enabled(&self, extension_id: &str) -> bool {
        let target = extension_id.trim();
        self.manifests
            .iter()
            .find(|m| m.extension_id == target)
            .map(|m| m.enabled)
            .unwrap_or(false)
    }
}

/// Result of an Admin Console `test_fetch` diagnostic run against a single extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionTestFetchResult {
    pub extension_id: String,
    pub ok: bool,
    pub health_status: String,
    pub latency_ms: u32,
    pub envelope_count: usize,
    pub protobuf_byte_len: usize,
    pub protobuf_base64: String,
    #[serde(skip)]
    pub protobuf_wire_bytes: Vec<u8>,
    pub canonical_json: Value,
    pub http_preview: Value,
    pub error: Option<String>,
}

struct HubInnerState {
    modules: BTreeMap<String, SystemModuleRow>,
    extensions: BTreeMap<String, RegisteredExtensionEntry>,
    cached_snapshot: Option<Arc<RuntimeRegistrySnapshot>>,
}

/// Central thread-safe registry & execution hub for Open Estate's modular data extensions.
pub struct ExtensionHub {
    state: RwLock<HubInnerState>,
    generation: AtomicU64,
    http_cache: Cache<String, CachedEnvelopeEntry>,
    shared_cache: crate::extensions::shared_cache::SharedExtensionCache,
    limiters: RwLock<HashMap<String, (u32, u32, Arc<DefaultDirectRateLimiter>)>>,
    self_weak: RwLock<Weak<ExtensionHub>>,
    hook_state_builder: RwLock<Option<HookStateBuilder>>,
}

static GLOBAL_EXTENSION_HUB: Lazy<Arc<ExtensionHub>> = Lazy::new(ExtensionHub::new_default);

impl ExtensionHub {
    /// Process-wide singleton `ExtensionHub` initialized with all built-in YAML/OpenAPI packages.
    pub fn global() -> &'static Arc<ExtensionHub> {
        &GLOBAL_EXTENSION_HUB
    }

    /// Creates a fresh `ExtensionHub` preloaded with `DEFAULT_SYSTEM_MODULES`, all embedded
    /// built-in extensions, and any additional `extensions/*/manifest.yaml` packages on disk.
    pub fn new_default() -> Arc<Self> {
        let hub = Arc::new(Self {
            state: RwLock::new(HubInnerState {
                modules: seed_default_modules_map(),
                extensions: BTreeMap::new(),
                cached_snapshot: None,
            }),
            generation: AtomicU64::new(1),
            http_cache: Cache::builder()
                .max_capacity(4_096)
                .time_to_live(Duration::from_secs(30 * 86_400))
                .build(),
            shared_cache: crate::extensions::shared_cache::SharedExtensionCache::new(),
            limiters: RwLock::new(HashMap::new()),
            self_weak: RwLock::new(Weak::new()),
            hook_state_builder: RwLock::new(None),
        });
        *hub
            .self_weak
            .write()
            .expect("ExtensionHub self_weak write") = Arc::downgrade(&hub);
        hub.load_builtin_packages();
        let default_dir = Path::new("extensions");
        if default_dir.is_dir() {
            let _ = hub.load_directory(default_dir, true);
        }
        hub.install_builtin_native_adapters();
        hub.refresh_snapshot();
        hub
    }

    /// Binds an `AppState` factory onto this hub (cycle-free via `Weak<ExtensionHub>`) so
    /// wave execution & `test_fetch` invoke `ServerExtensionHooks` authorization/metering.
    pub fn attach_hook_state_builder(&self, builder: HookStateBuilder) {
        let mut guard = self
            .hook_state_builder
            .write()
            .expect("ExtensionHub hook_state_builder write");
        *guard = Some(builder);
    }

    /// Runs `fut` with a task-local `AppState` override for hook authorization & metering.
    pub fn with_hook_state<F, T>(
        state: crate::web::server::AppState,
        fut: F,
    ) -> impl std::future::Future<Output = T>
    where
        F: std::future::Future<Output = T>,
    {
        ACTIVE_HOOK_STATE.scope(state, fut)
    }

    /// Returns the task-local [`crate::web::server::AppState`] bound via `ExtensionHub::with_hook_state`
    /// (or fallback `hook_state_builder` on the global hub).
    pub fn active_hook_state() -> Option<crate::web::server::AppState> {
        ACTIVE_HOOK_STATE
            .try_with(Clone::clone)
            .ok()
            .or_else(|| Self::global().resolve_hook_state())
    }

    fn resolve_hook_state(&self) -> Option<crate::web::server::AppState> {
        if let Ok(st) = ACTIVE_HOOK_STATE.try_with(Clone::clone) {
            return Some(st);
        }
        let hub_arc = self
            .self_weak
            .read()
            .expect("ExtensionHub self_weak read")
            .upgrade()?;
        let builder = self
            .hook_state_builder
            .read()
            .expect("ExtensionHub hook_state_builder read")
            .as_ref()
            .cloned()?;
        Some(builder(&hub_arc))
    }

    /// Returns the persistent + LRU shared cross-home data cache coordinator.
    pub fn shared_cache(&self) -> &crate::extensions::shared_cache::SharedExtensionCache {
        &self.shared_cache
    }

    /// Returns the active [`DbStore`] bound via `AppState` or task-local `ExtensionHub::with_hook_state`.
    pub fn active_db(&self) -> Option<DbStore> {
        self.resolve_hook_state().map(|s| s.db)
    }

    /// Registers native adapters provided by `ServerExtensionHooks::extra_extension_adapters`.
    pub fn register_hook_adapters(&self, adapters: Vec<Arc<dyn DataExtensionAdapter>>) {
        for adapter in adapters {
            self.register_native_adapter(adapter);
        }
    }

    /// Returns all registered manifests whose `execution_mode` is `"lazy"`.
    pub fn list_lazy_manifests(&self) -> Vec<ExtensionManifestProto> {
        self.list_manifests()
            .into_iter()
            .filter(manifest_is_lazy)
            .collect()
    }

    /// Idempotently enqueues all `execution_mode == "lazy"` extensions for `slug`/`report`
    /// using singleflight deduplication (`crate::lazy::enqueue`).
    pub async fn enqueue_lazy_for_report(
        &self,
        db: &DbStore,
        slug: &str,
        report: &Value,
        trigger: &str,
        requested_by_user_id: Option<&str>,
    ) -> PeriodicLazyEnqueueSummary {
        let mut summary = PeriodicLazyEnqueueSummary::default();
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return summary;
        }
        let specs = self.list_lazy_manifests();
        if specs.is_empty() {
            return summary;
        }

        let mut query = serde_json::Map::new();
        let addr = report
            .pointer("/property/address")
            .or_else(|| report.pointer("/meta/displayAddress"))
            .or_else(|| report.pointer("/geo/displayAddress"))
            .or_else(|| report.pointer("/input/address"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !addr.is_empty() {
            query.insert("address".to_string(), json!(addr));
        }
        for (ptr, key) in [
            ("/geo/city", "city"),
            ("/geo/state", "state"),
            ("/geo/zip", "zip"),
            ("/geo/countyFips", "county_fips"),
            ("/geo/countyName", "countyName"),
            ("/facts/apn", "apn"),
        ] {
            if let Some(val) = report.pointer(ptr).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                query.insert(key.to_string(), json!(val));
            }
        }
        if let Some(lat) = report.pointer("/geo/lat").and_then(Value::as_f64).filter(|v| v.is_finite()) {
            query.insert("lat".to_string(), json!(lat));
        }
        if let Some(lon) = report.pointer("/geo/lon").and_then(Value::as_f64).filter(|v| v.is_finite()) {
            query.insert("lon".to_string(), json!(lon));
        }
        let query_val = Value::Object(query);
        let clean_trigger = if trigger.trim().is_empty() {
            "first_visit"
        } else {
            trigger.trim()
        };

        summary.targets_examined = 1;
        for m in specs {
            summary.jobs_evaluated += 1;
            let raw_sec = manifest_section_id(&m);
            let section_id = if raw_sec.is_empty() {
                "publicResearch"
            } else {
                raw_sec
            };
            let req = EnqueueLazyJobRequest::new(clean_slug, &m.extension_id, section_id)
                .with_trigger(clean_trigger)
                .with_query(query_val.clone())
                .with_user_id(requested_by_user_id.map(ToOwned::to_owned));
            match enqueue_lazy_job(db, Some(self), req).await {
                Ok(out) => {
                    if out.queued {
                        summary.queued += 1;
                    } else if out.deduplicated {
                        summary.deduplicated += 1;
                    } else if out.section_status == LazySectionStatusKind::AwaitingApproval {
                        summary.awaiting_approval += 1;
                    } else if out.section_status == LazySectionStatusKind::NotEnabled {
                        summary.not_enabled += 1;
                    }
                }
                Err(err) => {
                    summary.errors += 1;
                    tracing::warn!(
                        slug = clean_slug,
                        extension_id = %m.extension_id,
                        error = %err,
                        "Lazy extension enqueue failed"
                    );
                }
            }
        }
        summary
    }

    /// Creates an `ExtensionHub` loading packages from an explicit directory path.
    pub fn from_dir(dir: &Path) -> Result<Arc<Self>> {
        let hub = Self::new_default();
        if dir.is_dir() {
            hub.load_directory(dir, true)?;
            hub.install_builtin_native_adapters();
            hub.refresh_snapshot();
        }
        Ok(hub)
    }

    fn install_builtin_native_adapters(&self) {
        use crate::modules::broadband_live::FccBroadbandBdcAdapter;
        use crate::modules::live_tariffs::all_live_tariff_adapters;
        use crate::modules::title_legal_radar::TitleUnderwriterRatesAdapter;
        use crate::property::permit_pipeline::{
            CampbellMgoPermitsAdapter, MunicipalOpenDataPermitsAdapter,
            PermitCodeEnforcementExtractorAdapter, PermitMepExtractorAdapter,
            PermitRemodelExtractorAdapter, PermitRoofExtractorAdapter, PermitSolarExtractorAdapter,
        };

        // Stage-1 Open-Data Permit Fetchers & Stage-2 Linked Extractors
        self.register_native_adapter(Arc::new(CampbellMgoPermitsAdapter));
        self.register_native_adapter(Arc::new(MunicipalOpenDataPermitsAdapter));
        self.register_native_adapter(Arc::new(PermitRoofExtractorAdapter));
        self.register_native_adapter(Arc::new(PermitSolarExtractorAdapter));
        self.register_native_adapter(Arc::new(PermitMepExtractorAdapter));
        self.register_native_adapter(Arc::new(PermitRemodelExtractorAdapter));
        self.register_native_adapter(Arc::new(PermitCodeEnforcementExtractorAdapter));

        // Real Live Utility & Open Broadband Adapters
        for adapter in all_live_tariff_adapters() {
            self.register_native_adapter(adapter);
        }
        self.register_native_adapter(Arc::new(FccBroadbandBdcAdapter));
        self.register_native_adapter(Arc::new(TitleUnderwriterRatesAdapter));
    }

    fn load_builtin_packages(&self) {
        let now = now_iso();
        let mut guard = self.state.write().expect("ExtensionHub write lock");
        for spec in EMBEDDED_BUILTINS {
            if let Ok(pkg) =
                parse_and_validate_package(spec.manifest_yaml, spec.openapi_yaml, Some(true))
            {
                let enabled = pkg.doc.enabled_by_default;
                let priority = pkg.doc.priority;
                let manifest = pkg.to_manifest_proto(enabled, priority);
                let health = if pkg.has_catalog_rules() && !pkg.has_live_http() {
                    "healthy"
                } else {
                    "ready"
                };
                let manifest_yaml = Some(pkg.manifest_yaml.clone());
                let openapi_yaml = pkg.openapi_yaml.clone();
                guard.extensions.insert(
                    pkg.extension_id.clone(),
                    RegisteredExtensionEntry {
                        extension_id: pkg.extension_id.clone(),
                        manifest,
                        driver_slot: Arc::new(ExtensionDriverSlot::DeclarativeOrOpenApi(Arc::new(
                            pkg,
                        ))),
                        manifest_yaml,
                        openapi_yaml,
                        config_json: "{}".to_string(),
                        health_status: health.to_string(),
                        last_checked_at: None,
                        installed_at: now.clone(),
                        updated_at: now.clone(),
                    },
                );
            }
        }
    }

    /// Scans `dir/*/manifest.yaml` (and optional sibling `openapi.yaml`) and registers packages.
    pub fn load_directory(&self, dir: &Path, mark_builtin: bool) -> Result<usize> {
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(0),
        };
        let now = now_iso();
        let mut loaded = 0usize;
        let mut parsed_list = Vec::new();

        for ent in entries.flatten() {
            let sub = ent.path();
            if !sub.is_dir() {
                continue;
            }
            let manifest_path = sub.join("manifest.yaml");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest_yaml = std::fs::read_to_string(&manifest_path).with_context(|| {
                format!("Failed reading {}", manifest_path.display())
            })?;
            let openapi_path = sub.join("openapi.yaml");
            let openapi_yaml = if openapi_path.is_file() {
                Some(std::fs::read_to_string(&openapi_path).with_context(|| {
                    format!("Failed reading {}", openapi_path.display())
                })?)
            } else {
                None
            };
            let pkg = parse_and_validate_package(
                &manifest_yaml,
                openapi_yaml.as_deref(),
                Some(mark_builtin),
            )
            .with_context(|| format!("Invalid extension package in {}", sub.display()))?;
            parsed_list.push(pkg);
        }

        if !parsed_list.is_empty() {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            for pkg in parsed_list {
                let preserve = guard
                    .extensions
                    .get(&pkg.extension_id)
                    .map(|e| (e.manifest.enabled, e.manifest.priority, e.installed_at.clone()));
                let (enabled, priority, installed_at) =
                    preserve.unwrap_or((pkg.doc.enabled_by_default, pkg.doc.priority, now.clone()));
                let manifest = pkg.to_manifest_proto(enabled, priority);
                let health = if pkg.has_catalog_rules() && !pkg.has_live_http() {
                    "healthy"
                } else {
                    "ready"
                };
                let manifest_yaml = Some(pkg.manifest_yaml.clone());
                let openapi_yaml = pkg.openapi_yaml.clone();
                let preserve_native_slot = if pkg.driver == DriverKind::DriverNativeAdapter {
                    guard.extensions.get(&pkg.extension_id).and_then(|existing| {
                        match existing.driver_slot.as_ref() {
                            ExtensionDriverSlot::Native(_) => {
                                Some((Arc::clone(&existing.driver_slot), existing.health_status.clone()))
                            }
                            _ => None,
                        }
                    })
                } else {
                    None
                };
                let (driver_slot, final_health) = match preserve_native_slot {
                    Some((slot, prev_health)) => (slot, prev_health),
                    None => (
                        Arc::new(ExtensionDriverSlot::DeclarativeOrOpenApi(Arc::new(pkg))),
                        health.to_string(),
                    ),
                };
                let ext_id = manifest.extension_id.clone();
                guard.extensions.insert(
                    ext_id.clone(),
                    RegisteredExtensionEntry {
                        extension_id: ext_id,
                        manifest,
                        driver_slot,
                        manifest_yaml,
                        openapi_yaml,
                        config_json: "{}".to_string(),
                        health_status: final_health,
                        last_checked_at: None,
                        installed_at,
                        updated_at: now.clone(),
                    },
                );
                loaded += 1;
            }
            guard.cached_snapshot = None;
            self.generation.fetch_add(1, Ordering::SeqCst);
        }

        Ok(loaded)
    }

    /// Current monotonic configuration generation version (bumped on every module or extension change).
    pub fn generation_version(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn bump_and_refresh(&self) -> Arc<RuntimeRegistrySnapshot> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.refresh_snapshot()
    }

    fn refresh_snapshot(&self) -> Arc<RuntimeRegistrySnapshot> {
        let gen_v = self.generation.load(Ordering::SeqCst);
        let mut guard = self.state.write().expect("ExtensionHub write lock");
        let snap = Arc::new(build_snapshot_from_state(&guard, gen_v));
        guard.cached_snapshot = Some(Arc::clone(&snap));
        snap
    }

    /// Returns an immutable `Arc<RuntimeRegistrySnapshot>` for fast lock-free per-request checks.
    pub fn snapshot(&self) -> Arc<RuntimeRegistrySnapshot> {
        {
            let guard = self.state.read().expect("ExtensionHub read lock");
            if let Some(ref snap) = guard.cached_snapshot {
                return Arc::clone(snap);
            }
        }
        self.refresh_snapshot()
    }

    /// Checks whether a core `system_module` (e.g. `"utilities_broadband"`) is enabled.
    pub fn is_module_enabled(&self, module_id: &str) -> bool {
        let clean = module_id.trim();
        if clean == "geocode" {
            return true;
        }
        let guard = self.state.read().expect("ExtensionHub read lock");
        guard
            .modules
            .get(clean)
            .map(|m| m.enabled)
            .unwrap_or(true)
    }

    /// Enables or disables a core `system_module` at runtime and bumps `generation_version`.
    pub fn set_module_enabled(&self, module_id: &str, enabled: bool) -> Result<bool> {
        let clean = module_id.trim();
        if clean == "geocode" && !enabled {
            return Err(anyhow!("Core 'geocode' module cannot be disabled"));
        }
        let found = {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            if let Some(m) = guard.modules.get_mut(clean) {
                m.enabled = enabled;
                m.updated_at = now_iso();
                guard.cached_snapshot = None;
                true
            } else {
                false
            }
        };
        if found {
            self.bump_and_refresh();
        }
        Ok(found)
    }

    /// Checks whether a specific `extension_id` is currently registered and enabled.
    pub fn is_enabled(&self, extension_id: &str) -> bool {
        let guard = self.state.read().expect("ExtensionHub read lock");
        guard
            .extensions
            .get(extension_id.trim())
            .map(|e| e.manifest.enabled)
            .unwrap_or(false)
    }

    /// Enables or disables an extension at runtime (`zero restart`).
    pub fn enable(&self, extension_id: &str, enabled: bool) -> Result<ExtensionManifestProto> {
        let updated = {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            let manifest = {
                let entry = guard
                    .extensions
                    .get_mut(extension_id.trim())
                    .ok_or_else(|| anyhow!("Extension '{}' not found", extension_id.trim()))?;
                entry.manifest.enabled = enabled;
                entry.updated_at = now_iso();
                entry.manifest.clone()
            };
            guard.cached_snapshot = None;
            manifest
        };
        self.bump_and_refresh();
        Ok(updated)
    }

    /// Updates an extension's deterministic resolution priority (`0..=10000`) at runtime.
    pub fn set_priority(
        &self,
        extension_id: &str,
        priority: i32,
    ) -> Result<ExtensionManifestProto> {
        let clamped = priority.clamp(0, 10_000);
        let updated = {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            let manifest = {
                let entry = guard
                    .extensions
                    .get_mut(extension_id.trim())
                    .ok_or_else(|| anyhow!("Extension '{}' not found", extension_id.trim()))?;
                entry.manifest.priority = clamped;
                entry.updated_at = now_iso();
                entry.manifest.clone()
            };
            guard.cached_snapshot = None;
            manifest
        };
        self.bump_and_refresh();
        Ok(updated)
    }

    /// Validates and hot-installs (or updates) a custom YAML + optional OpenAPI 3.x extension.
    pub fn install_yaml(
        &self,
        manifest_yaml: &str,
        openapi_yaml: Option<&str>,
        config_json: Option<&str>,
    ) -> Result<ExtensionManifestProto> {
        let pkg = parse_and_validate_package(manifest_yaml, openapi_yaml, Some(false))?;
        let now = now_iso();
        let cfg = config_json
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("{}")
            .to_string();

        let manifest = {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            let is_existing_builtin = guard
                .extensions
                .get(&pkg.extension_id)
                .map(|e| e.manifest.builtin)
                .unwrap_or(false);
            let mut pkg_mut = pkg;
            pkg_mut.builtin = is_existing_builtin;
            let manifest =
                pkg_mut.to_manifest_proto(pkg_mut.doc.enabled_by_default, pkg_mut.doc.priority);

            // Reject cyclic DAG dependency graphs prior to mutating registry state.
            if let Some(cycle) = detect_dag_cycle_for_candidate(&guard.extensions, &manifest) {
                return Err(anyhow!(
                    "Cyclic extension dependency detected for '{}': {}",
                    manifest.extension_id,
                    cycle.join(" -> ")
                ));
            }

            let installed_at = guard
                .extensions
                .get(&pkg_mut.extension_id)
                .map(|e| e.installed_at.clone())
                .unwrap_or_else(|| now.clone());
            let health = if pkg_mut.has_catalog_rules() && !pkg_mut.has_live_http() {
                "healthy"
            } else {
                "ready"
            };
            let manifest_yaml_owned = Some(pkg_mut.manifest_yaml.clone());
            let openapi_yaml_owned = pkg_mut.openapi_yaml.clone();
            guard.extensions.insert(
                pkg_mut.extension_id.clone(),
                RegisteredExtensionEntry {
                    extension_id: pkg_mut.extension_id.clone(),
                    manifest: manifest.clone(),
                    driver_slot: Arc::new(ExtensionDriverSlot::DeclarativeOrOpenApi(Arc::new(
                        pkg_mut,
                    ))),
                    manifest_yaml: manifest_yaml_owned,
                    openapi_yaml: openapi_yaml_owned,
                    config_json: cfg,
                    health_status: health.to_string(),
                    last_checked_at: None,
                    installed_at,
                    updated_at: now,
                },
            );
            guard.cached_snapshot = None;
            manifest
        };

        self.bump_and_refresh();
        Ok(manifest)
    }

    /// Validates and registers a built-in overlay YAML + optional OpenAPI 3.x extension package
    /// supplied by `ServerExtensionHooks::extra_extension_packages()`, preserving `builtin = true`
    /// so overlay packages cannot be accidentally uninstalled via the admin API.
    pub fn register_hook_package(
        &self,
        manifest_yaml: &str,
        openapi_yaml: Option<&str>,
    ) -> Result<ExtensionManifestProto> {
        let pkg = parse_and_validate_package(manifest_yaml, openapi_yaml, Some(true))?;
        let now = now_iso();
        let manifest = {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            let mut pkg_mut = pkg;
            pkg_mut.builtin = true;
            let manifest =
                pkg_mut.to_manifest_proto(pkg_mut.doc.enabled_by_default, pkg_mut.doc.priority);

            if let Some(cycle) = detect_dag_cycle_for_candidate(&guard.extensions, &manifest) {
                return Err(anyhow!(
                    "Cyclic extension dependency detected for '{}': {}",
                    manifest.extension_id,
                    cycle.join(" -> ")
                ));
            }

            let installed_at = guard
                .extensions
                .get(&pkg_mut.extension_id)
                .map(|e| e.installed_at.clone())
                .unwrap_or_else(|| now.clone());
            let health = if pkg_mut.has_catalog_rules() && !pkg_mut.has_live_http() {
                "healthy"
            } else {
                "ready"
            };
            let manifest_yaml_owned = Some(pkg_mut.manifest_yaml.clone());
            let openapi_yaml_owned = pkg_mut.openapi_yaml.clone();
            guard.extensions.insert(
                pkg_mut.extension_id.clone(),
                RegisteredExtensionEntry {
                    extension_id: pkg_mut.extension_id.clone(),
                    manifest: manifest.clone(),
                    driver_slot: Arc::new(ExtensionDriverSlot::DeclarativeOrOpenApi(Arc::new(
                        pkg_mut,
                    ))),
                    manifest_yaml: manifest_yaml_owned,
                    openapi_yaml: openapi_yaml_owned,
                    config_json: "{}".to_string(),
                    health_status: health.to_string(),
                    last_checked_at: None,
                    installed_at,
                    updated_at: now,
                },
            );
            guard.cached_snapshot = None;
            manifest
        };

        self.bump_and_refresh();
        Ok(manifest)
    }

    /// Removes a custom extension (`builtin == false`). Built-in packages cannot be deleted, only disabled.
    pub fn uninstall(&self, extension_id: &str) -> Result<bool> {
        let clean = extension_id.trim();
        let removed = {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            if let Some(existing) = guard.extensions.get(clean) {
                if existing.manifest.builtin {
                    return Err(anyhow!(
                        "Built-in extension '{clean}' cannot be uninstalled; disable it instead"
                    ));
                }
            }
            let r = guard.extensions.remove(clean).is_some();
            if r {
                guard.cached_snapshot = None;
            }
            r
        };
        if removed {
            self.bump_and_refresh();
        }
        Ok(removed)
    }

    /// Registers a compiled native Rust `DataExtensionAdapter` (`DRIVER_NATIVE_ADAPTER`).
    pub fn register_native_adapter(&self, adapter: Arc<dyn DataExtensionAdapter>) {
        let mut manifest = adapter.manifest();
        let now = now_iso();
        {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            let (existing_manifest_yaml, existing_openapi_yaml, preserved_cfg, installed_at) =
                if let Some(prev) = guard.extensions.get(&manifest.extension_id) {
                    if prev.manifest.builtin {
                        manifest.builtin = true;
                        manifest.enabled = prev.manifest.enabled;
                        manifest.priority = prev.manifest.priority;
                    }
                    if manifest.depends_on.is_empty() && !prev.manifest.depends_on.is_empty() {
                        manifest.depends_on = prev.manifest.depends_on.clone();
                    }
                    if manifest.input_from_domains.is_empty()
                        && !prev.manifest.input_from_domains.is_empty()
                    {
                        manifest.input_from_domains = prev.manifest.input_from_domains.clone();
                    }
                    if manifest.upstream_require_mode.is_empty()
                        && !prev.manifest.upstream_require_mode.is_empty()
                    {
                        manifest.upstream_require_mode =
                            prev.manifest.upstream_require_mode.clone();
                    }
                    if manifest.tier == 0 && prev.manifest.tier != 0 {
                        manifest.tier = prev.manifest.tier;
                    }
                    if manifest.license_class == 0 && prev.manifest.license_class != 0 {
                        manifest.license_class = prev.manifest.license_class;
                    }
                    if manifest.execution_mode == 0 && prev.manifest.execution_mode != 0 {
                        manifest.execution_mode = prev.manifest.execution_mode;
                    }
                    if prev.manifest.requires_manual_approval {
                        manifest.requires_manual_approval = true;
                    }
                    if manifest.min_plan_tier.trim().is_empty()
                        && !prev.manifest.min_plan_tier.trim().is_empty()
                    {
                        manifest.min_plan_tier = prev.manifest.min_plan_tier.clone();
                    }
                    if manifest.metering.is_none() && prev.manifest.metering.is_some() {
                        manifest.metering = prev.manifest.metering.clone();
                    }
                    if manifest.section_id.trim().is_empty()
                        && !prev.manifest.section_id.trim().is_empty()
                    {
                        manifest.section_id = prev.manifest.section_id.clone();
                    }
                    if manifest.enabled_by_default.is_none() {
                        manifest.enabled_by_default = prev.manifest.enabled_by_default;
                    }
                    if manifest.llm.is_none() {
                        manifest.llm = prev.manifest.llm.clone();
                    }
                    if manifest.sharing.is_none() {
                        manifest.sharing = prev.manifest.sharing.clone();
                    }
                    if manifest.source_attribution.is_none()
                        && prev.manifest.source_attribution.is_some()
                    {
                        manifest.source_attribution = prev.manifest.source_attribution.clone();
                    }
                    if manifest
                        .homepage_url
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                        && prev
                            .manifest
                            .homepage_url
                            .as_deref()
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                    {
                        manifest.homepage_url = prev.manifest.homepage_url.clone();
                    }
                    if manifest
                        .documentation_url
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                        && prev
                            .manifest
                            .documentation_url
                            .as_deref()
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                    {
                        manifest.documentation_url = prev.manifest.documentation_url.clone();
                    }
                    (
                        prev.manifest_yaml.clone(),
                        prev.openapi_yaml.clone(),
                        prev.config_json.clone(),
                        prev.installed_at.clone(),
                    )
                } else {
                    (None, None, "{}".to_string(), now.clone())
                };
            guard.extensions.insert(
                manifest.extension_id.clone(),
                RegisteredExtensionEntry {
                    extension_id: manifest.extension_id.clone(),
                    manifest,
                    driver_slot: Arc::new(ExtensionDriverSlot::Native(adapter)),
                    manifest_yaml: existing_manifest_yaml,
                    openapi_yaml: existing_openapi_yaml,
                    config_json: preserved_cfg,
                    health_status: "healthy".to_string(),
                    last_checked_at: None,
                    installed_at,
                    updated_at: now,
                },
            );
            guard.cached_snapshot = None;
        }
        self.bump_and_refresh();
    }

    /// Lists all registered `ExtensionManifestProto` messages sorted by `(domain, -priority, id)`.
    pub fn list_manifests(&self) -> Vec<ExtensionManifestProto> {
        let guard = self.state.read().expect("ExtensionHub read lock");
        let mut out: Vec<ExtensionManifestProto> = guard
            .extensions
            .values()
            .map(|e| e.manifest.clone())
            .collect();
        out.sort_by(|a, b| {
            a.domain
                .cmp(&b.domain)
                .then_with(|| b.priority.cmp(&a.priority))
                .then_with(|| a.extension_id.cmp(&b.extension_id))
        });
        out
    }

    /// Resolves a live reference URL for `extension_id` (and optional catalog `rule_id`) using
    /// the extension's current `ExtensionManifestProto` (falling back to `ExtensionHub::global()`).
    pub fn resolve_extension_reference_url(
        &self,
        extension_id: &str,
        rule_id: Option<&str>,
        query: &ExtensionQueryProto,
    ) -> Option<String> {
        let raw_id = extension_id.trim();
        if raw_id.is_empty() {
            return None;
        }
        let clean_id = match raw_id {
            "fcc_broadband_bdc_live" => "fcc_broadband_bdc",
            "epa_echo_sdwis" | "sdwis_water" => "epa_sdwis_water",
            "usgs_asce7" => "usgs_asce7_seismic",
            other => other,
        };
        let manifest_opt = {
            let guard = self.state.read().expect("ExtensionHub read lock");
            guard.extensions.get(clean_id).map(|e| e.manifest.clone())
        }
        .or_else(|| {
            let guard = Self::global().state.read().expect("ExtensionHub read lock");
            guard.extensions.get(clean_id).map(|e| e.manifest.clone())
        });
        let manifest = manifest_opt?;
        crate::extensions::yaml_runner::resolve_manifest_proto_reference_url(
            &manifest, rule_id, query,
        )
    }

    /// Collects all unique lowercase hostnames declared across registered extension manifests
    /// (`homepage_url`, `documentation_url`, `source_attribution`, and catalog rules).
    pub fn registered_reference_hosts(&self) -> Vec<String> {
        let mut hosts = std::collections::BTreeSet::new();
        let mut collect_from = |hub: &ExtensionHub| {
            let guard = hub.state.read().expect("ExtensionHub read lock");
            for entry in guard.extensions.values() {
                let m = &entry.manifest;
                let mut candidates: Vec<&str> = Vec::new();
                if let Some(ref u) = m.homepage_url {
                    candidates.push(u.as_str());
                }
                if let Some(ref u) = m.documentation_url {
                    candidates.push(u.as_str());
                }
                if let Some(ref sa) = m.source_attribution {
                    candidates.push(sa.url.as_str());
                    candidates.push(sa.reference_url_template.as_str());
                }
                if let Some(ref tc) = m.tariff_catalog {
                    for r in &tc.electric_rules {
                        candidates.push(r.tariff_url.as_str());
                    }
                    for r in &tc.gas_rules {
                        candidates.push(r.tariff_url.as_str());
                    }
                    for r in &tc.water_sewer_trash_rules {
                        if let Some(ref u) = r.water_tariff_url {
                            candidates.push(u.as_str());
                        }
                    }
                }
                if let Some(ref fc) = m.footprint_catalog {
                    for r in &fc.broadband_rules {
                        candidates.push(r.check_url.as_str());
                    }
                }
                for raw in candidates {
                    let clean_tpl = raw.split("?").next().unwrap_or(raw).trim();
                    if let Ok(parsed) = url::Url::parse(clean_tpl) {
                        if let Some(h) = parsed.host_str() {
                            hosts.insert(h.to_ascii_lowercase());
                        }
                    }
                }
            }
        };
        collect_from(self);
        collect_from(Self::global());
        hosts.into_iter().collect()
    }

    /// Rich JSON representation of all registered extensions for `/api/admin/extensions`.
    pub fn list_extensions_json(&self, env: Option<&EnvConfig>) -> Vec<Value> {
        let guard = self.state.read().expect("ExtensionHub read lock");
        let mut entries: Vec<&RegisteredExtensionEntry> = guard.extensions.values().collect();
        entries.sort_by(|a, b| {
            b.manifest
                .priority
                .cmp(&a.manifest.priority)
                .then_with(|| a.extension_id.cmp(&b.extension_id))
        });
        entries
            .into_iter()
            .map(|e| entry_to_admin_json(e, env, false))
            .collect()
    }

    /// Rich JSON representation (including full `manifestYaml` and `openapiYaml`) for a single extension.
    pub fn get_extension_json(&self, extension_id: &str, env: Option<&EnvConfig>) -> Option<Value> {
        let guard = self.state.read().expect("ExtensionHub read lock");
        guard
            .extensions
            .get(extension_id.trim())
            .map(|e| entry_to_admin_json(e, env, true))
    }

    /// Synchronously resolves enabled extensions for `domain` in DAG topological order.
    pub fn execute_domain_sync(
        &self,
        domain: CapabilityDomain,
        query: &ExtensionQueryProto,
    ) -> Vec<ExtensionEnvelopeProto> {
        let (envelopes, _, _) = self.execute_waves_sync(Some(&[domain]), query);
        filter_envelopes_for_domains(&envelopes, Some(&[domain]))
    }

    /// Synchronously resolves and aggregates multiple capability domains in DAG topological waves.
    pub fn execute_domains_sync(
        &self,
        domains: &[CapabilityDomain],
        query: &ExtensionQueryProto,
    ) -> AggregatedPropertyBundleProto {
        let (all_envelopes, max_wave_by_domain, skipped_by_domain) =
            self.execute_waves_sync(Some(domains), query);
        let scoped = filter_envelopes_for_domains(&all_envelopes, Some(domains));
        let mut bundle = aggregate_envelopes(query, &scoped, None);
        apply_wave_trace_metadata(&mut bundle, &max_wave_by_domain, &skipped_by_domain);
        bundle
    }

    /// Asynchronously executes all enabled extensions (OpenAPI 3.x, declarative HTTP, catalog rules,
    /// and native adapters) for `domain` in DAG topological waves.
    pub async fn execute_domain(
        &self,
        domain: CapabilityDomain,
        query: &ExtensionQueryProto,
        http: &HttpClient,
        env: &EnvConfig,
    ) -> Vec<ExtensionEnvelopeProto> {
        let (envelopes, _, _) = self.execute_waves_async(Some(&[domain]), query, http, env).await;
        filter_envelopes_for_domains(&envelopes, Some(&[domain]))
    }

    /// Asynchronously executes enabled extensions across `domains` (or all domains when `None`)
    /// in DAG topological wave order, skipping downstream extractors when upstream fails/is disabled,
    /// and deterministically aggregates the resulting `ExtensionEnvelopeProto` messages.
    pub async fn resolve_all(
        &self,
        query: &ExtensionQueryProto,
        domains: Option<&[CapabilityDomain]>,
        http: &HttpClient,
        env: &EnvConfig,
    ) -> AggregatedPropertyBundleProto {
        let (envelopes, max_wave_by_domain, skipped_by_domain) =
            self.execute_waves_async(domains, query, http, env).await;
        let scoped = filter_envelopes_for_domains(&envelopes, domains);
        let mut bundle = aggregate_envelopes(query, &scoped, None);
        apply_wave_trace_metadata(&mut bundle, &max_wave_by_domain, &skipped_by_domain);
        bundle
    }

    fn invocation_ctx_for(
        manifest: &ExtensionManifestProto,
        trigger: InvocationTrigger,
        parent_invocation_id: Option<String>,
        query: Option<&ExtensionQueryProto>,
    ) -> InvocationContext {
        let mut ctx = InvocationContext::new(&manifest.extension_id)
            .with_version(&manifest.version)
            .with_tier(ExtensionTier::from_str_lossy(manifest_tier(manifest)))
            .with_trigger(trigger);
        if let Some(pid) = parent_invocation_id {
            ctx = ctx.with_parent_invocation_id(pid);
        }
        if let Some(llm_spec) = manifest_to_active_llm_spec(manifest) {
            ctx = ctx.with_llm_spec(llm_spec);
        }
        if let Some(q) = query {
            if let Some(slug) = q
                .extra_params
                .get("slug")
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                ctx = ctx.with_slug(slug);
            }
            if let Some(uid) = q
                .extra_params
                .get("requested_by_user_id")
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                ctx = ctx.with_actor_user_id(uid);
            }
        }
        ctx
    }

    fn default_authorize_manifest(
        ctx: &InvocationContext,
        manifest: &ExtensionManifestProto,
    ) -> InvocationDecision {
        default_oss_authorize(
            ctx,
            (manifest.tier != 0).then(|| manifest_tier(manifest)),
            (manifest.license_class != 0).then(|| manifest_license_class(manifest)),
            manifest_min_plan_tier(manifest),
            manifest.requires_manual_approval,
        )
    }

    async fn authorize_invocation(
        &self,
        ctx: &InvocationContext,
        manifest: &ExtensionManifestProto,
    ) -> InvocationDecision {
        if let Some(hook_state) = self.resolve_hook_state() {
            return hook_state
                .hooks
                .authorize_extension_invocation(&hook_state, ctx, manifest)
                .await;
        }
        Self::default_authorize_manifest(ctx, manifest)
    }

    async fn record_usage_event(&self, event: &ExtensionUsageEvent) {
        if let Some(hook_state) = self.resolve_hook_state() {
            if let Err(err) = hook_state
                .hooks
                .record_extension_usage(&hook_state, event)
                .await
            {
                tracing::warn!(
                    extension_id = %event.extension_id,
                    error = %err,
                    "Failed to record extension usage event via ServerExtensionHooks"
                );
            }
        }
    }

    fn execute_waves_sync(
        &self,
        domains: Option<&[CapabilityDomain]>,
        query: &ExtensionQueryProto,
    ) -> (
        Vec<ExtensionEnvelopeProto>,
        HashMap<i32, u32>,
        HashMap<i32, Vec<String>>,
    ) {
        let plan = self.plan_execution_waves(domains);
        let mut envelopes_by_id: HashMap<String, Vec<ExtensionEnvelopeProto>> = HashMap::new();
        let mut invocation_ids_by_ext: HashMap<String, String> = HashMap::new();
        let mut succeeded_ids: HashSet<String> = HashSet::new();
        let mut max_wave_by_domain: HashMap<i32, u32> = HashMap::new();
        let mut skipped_by_domain: HashMap<i32, Vec<String>> = HashMap::new();

        for (wave_idx, wave) in plan.waves.into_iter().enumerate() {
            for planned in wave {
                let entry = planned.entry;
                let dom = entry.manifest.domain;
                if let Some(upstream_envs) = collect_upstream_if_satisfied(
                    &entry.manifest,
                    &planned.upstream_ids,
                    &succeeded_ids,
                    &envelopes_by_id,
                    &query.upstream_envelopes,
                ) {
                    let mut sub_query = query.clone();
                    if sub_query.upstream_envelopes.is_empty() && !upstream_envs.is_empty() {
                        sub_query.upstream_envelopes = upstream_envs.clone();
                    }
                    let trigger = if wave_idx > 0 && !planned.upstream_ids.is_empty() {
                        InvocationTrigger::Dependency
                    } else {
                        InvocationTrigger::UserRequest
                    };
                    let parent_inv_id = planned
                        .upstream_ids
                        .iter()
                        .find_map(|up_id| invocation_ids_by_ext.get(up_id).cloned());
                    let ctx = Self::invocation_ctx_for(
                        &entry.manifest,
                        trigger,
                        parent_inv_id,
                        Some(&sub_query),
                    );
                    let (exec_res, event) = MeterScope::run_sync(ctx.clone(), || {
                        let dec = Self::default_authorize_manifest(&ctx, &entry.manifest);
                        if !matches!(dec, InvocationDecision::Allow) {
                            mark_denied();
                            return Err(anyhow!("extension invocation denied"));
                        }
                        let res = match entry.driver_slot.as_ref() {
                            ExtensionDriverSlot::DeclarativeOrOpenApi(pkg) => {
                                if pkg.has_catalog_rules() {
                                    Ok(execute_catalog_rules(pkg, &entry.manifest, &sub_query))
                                } else if has_declared_upstream(&entry.manifest)
                                    && !pkg.has_live_http()
                                {
                                    execute_upstream_yaml_mapper(pkg, &entry.manifest, &sub_query)
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            ExtensionDriverSlot::Native(adapter) => {
                                adapter.execute_with_upstream_sync(&sub_query, &upstream_envs)
                            }
                        };
                        if res.is_err() {
                            mark_failed();
                        }
                        res
                    });
                    invocation_ids_by_ext
                        .insert(entry.extension_id.clone(), event.invocation_id.clone());

                    if let Ok(envs) = exec_res {
                        succeeded_ids.insert(entry.extension_id.clone());
                        if !envs.is_empty() {
                            let w = max_wave_by_domain.entry(dom).or_insert(0);
                            *w = (*w).max(wave_idx as u32);
                        }
                        envelopes_by_id.insert(entry.extension_id.clone(), envs);
                    } else {
                        skipped_by_domain
                            .entry(dom)
                            .or_default()
                            .push(entry.extension_id.clone());
                    }
                } else {
                    skipped_by_domain
                        .entry(dom)
                        .or_default()
                        .push(entry.extension_id.clone());
                }
            }
        }

        let all_envelopes: Vec<ExtensionEnvelopeProto> =
            envelopes_by_id.into_values().flatten().collect();
        (all_envelopes, max_wave_by_domain, skipped_by_domain)
    }

    async fn execute_waves_async(
        &self,
        domains: Option<&[CapabilityDomain]>,
        query: &ExtensionQueryProto,
        http: &HttpClient,
        env: &EnvConfig,
    ) -> (
        Vec<ExtensionEnvelopeProto>,
        HashMap<i32, u32>,
        HashMap<i32, Vec<String>>,
    ) {
        let plan = self.plan_execution_waves(domains);
        let mut envelopes_by_id: HashMap<String, Vec<ExtensionEnvelopeProto>> = HashMap::new();
        let mut invocation_ids_by_ext: HashMap<String, String> = HashMap::new();
        let mut succeeded_ids: HashSet<String> = HashSet::new();
        let mut max_wave_by_domain: HashMap<i32, u32> = HashMap::new();
        let mut skipped_by_domain: HashMap<i32, Vec<String>> = HashMap::new();

        for (wave_idx, wave) in plan.waves.into_iter().enumerate() {
            let mut runnable = Vec::new();
            for planned in wave {
                let dom = planned.entry.manifest.domain;
                if let Some(upstream_envs) = collect_upstream_if_satisfied(
                    &planned.entry.manifest,
                    &planned.upstream_ids,
                    &succeeded_ids,
                    &envelopes_by_id,
                    &query.upstream_envelopes,
                ) {
                    runnable.push((planned.entry, planned.upstream_ids, upstream_envs));
                } else {
                    skipped_by_domain
                        .entry(dom)
                        .or_default()
                        .push(planned.entry.extension_id.clone());
                }
            }

            let inv_map_snapshot = &invocation_ids_by_ext;
            let futs = runnable
                .into_iter()
                .map(|(entry, upstream_ids, upstream_envs)| async move {
                    let id = entry.extension_id.clone();
                    let dom = entry.manifest.domain;
                    let mut sub_query = query.clone();
                    if sub_query.upstream_envelopes.is_empty() && !upstream_envs.is_empty() {
                        sub_query.upstream_envelopes = upstream_envs.clone();
                    }
                    let trigger = if wave_idx > 0 && !upstream_ids.is_empty() {
                        InvocationTrigger::Dependency
                    } else {
                        InvocationTrigger::UserRequest
                    };
                    let parent_inv_id = upstream_ids
                        .iter()
                        .find_map(|up_id| inv_map_snapshot.get(up_id).cloned());
                    let ctx = Self::invocation_ctx_for(
                        &entry.manifest,
                        trigger,
                        parent_inv_id,
                        Some(&sub_query),
                    );
                    let (res, event) = MeterScope::run(ctx.clone(), async {
                        let dec = self.authorize_invocation(&ctx, &entry.manifest).await;
                        if !matches!(dec, InvocationDecision::Allow) {
                            mark_denied();
                            return Ok(None);
                        }
                        let exec_res = self
                            .execute_single_entry_internal(
                                &entry,
                                &sub_query,
                                &upstream_envs,
                                http,
                                env,
                                false,
                            )
                            .await;
                        match &exec_res {
                            Err(_) => mark_failed(),
                            Ok(Some(envs))
                                if !envs.is_empty()
                                    && envs.iter().all(|e| e.cached)
                                    && crate::metering::current_dims()
                                        .map(|d| d.external_api_calls == 0)
                                        .unwrap_or(false) =>
                            {
                                mark_cached();
                            }
                            _ => {}
                        }
                        exec_res
                    })
                    .await;
                    self.record_usage_event(&event).await;
                    (id, dom, res, event.invocation_id)
                });

            for (id, dom, res, inv_id) in join_all(futs).await {
                invocation_ids_by_ext.insert(id.clone(), inv_id);
                match res {
                    Ok(Some(envs)) => {
                        succeeded_ids.insert(id.clone());
                        if !envs.is_empty() {
                            let w = max_wave_by_domain.entry(dom).or_insert(0);
                            *w = (*w).max(wave_idx as u32);
                        }
                        envelopes_by_id.insert(id, envs);
                    }
                    Ok(None) | Err(_) => {
                        skipped_by_domain.entry(dom).or_default().push(id);
                    }
                }
            }
        }

        let all_envelopes: Vec<ExtensionEnvelopeProto> =
            envelopes_by_id.into_values().flatten().collect();
        (all_envelopes, max_wave_by_domain, skipped_by_domain)
    }

    fn plan_execution_waves(&self, domains: Option<&[CapabilityDomain]>) -> ExecutionWavePlan {
        let guard = self.state.read().expect("ExtensionHub read lock");
        let enabled_entries: HashMap<String, RegisteredExtensionEntry> = guard
            .extensions
            .iter()
            .filter(|(_, e)| {
                if !e.manifest.enabled || manifest_is_lazy(&e.manifest) {
                    return false;
                }
                let prim = CapabilityDomain::try_from(e.manifest.domain)
                    .unwrap_or(CapabilityDomain::DomainUnspecified);
                self.is_domain_module_enabled_with_map(&guard.modules, prim)
            })
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();

        // Start from entries matching requested domain(s), then include transitive upstream dependencies.
        let mut selected: HashSet<String> = enabled_entries
            .values()
            .filter(|e| entry_matches_domains(e, domains))
            .map(|e| e.extension_id.clone())
            .collect();

        let mut stack: Vec<String> = selected.iter().cloned().collect();
        while let Some(curr_id) = stack.pop() {
            if let Some(entry) = enabled_entries.get(&curr_id) {
                for up_id in resolve_declared_upstream_ids(&entry.manifest, &enabled_entries) {
                    if selected.insert(up_id.clone()) {
                        stack.push(up_id);
                    }
                }
            }
        }

        let mut deps_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut remaining: HashSet<String> = selected.clone();
        for id in &selected {
            if let Some(entry) = enabled_entries.get(id) {
                let ups = resolve_declared_upstream_ids(&entry.manifest, &enabled_entries)
                    .into_iter()
                    .filter(|u| selected.contains(u) && u != id)
                    .collect();
                deps_map.insert(id.clone(), ups);
            }
        }

        let mut waves: Vec<Vec<PlannedEntry>> = Vec::new();
        let mut done: HashSet<String> = HashSet::new();
        while !remaining.is_empty() {
            let mut ready: Vec<PlannedEntry> = remaining
                .iter()
                .filter_map(|id| {
                    let ups = deps_map.get(id)?;
                    if ups.iter().all(|u| done.contains(u)) {
                        let entry = enabled_entries.get(id)?.clone();
                        Some(PlannedEntry {
                            entry,
                            upstream_ids: ups.clone(),
                        })
                    } else {
                        None
                    }
                })
                .collect();

            if ready.is_empty() {
                // Defensive guard against unexpected cycles: break gracefully rather than looping.
                break;
            }
            ready.sort_by(|a, b| {
                b.entry
                    .manifest
                    .priority
                    .cmp(&a.entry.manifest.priority)
                    .then_with(|| a.entry.extension_id.cmp(&b.entry.extension_id))
            });
            for item in &ready {
                remaining.remove(&item.entry.extension_id);
                done.insert(item.entry.extension_id.clone());
            }
            waves.push(ready);
        }

        ExecutionWavePlan { waves }
    }

    fn limiter_for(&self, manifest: &ExtensionManifestProto) -> Arc<DefaultDirectRateLimiter> {
        let rps = manifest
            .rate_limit
            .as_ref()
            .map(|r| r.requests_per_second)
            .filter(|&n| n > 0)
            .unwrap_or(5);
        let burst = manifest
            .rate_limit
            .as_ref()
            .map(|r| r.burst)
            .filter(|&n| n > 0)
            .unwrap_or(rps.saturating_mul(2));
        {
            let guard = self.limiters.read().expect("ExtensionHub limiter read");
            if let Some((prev_rps, prev_burst, lim)) = guard.get(&manifest.extension_id) {
                if *prev_rps == rps && *prev_burst == burst {
                    return Arc::clone(lim);
                }
            }
        }
        let lim = Arc::new(build_extension_rate_limiter(manifest.rate_limit.as_ref()));
        let mut guard = self.limiters.write().expect("ExtensionHub limiter write");
        guard.insert(manifest.extension_id.clone(), (rps, burst, Arc::clone(&lim)));
        lim
    }

    /// Runs a live diagnostic `test_fetch` against a single extension (even if currently disabled),
    /// returning binary Protobuf wire bytes (`prost`), Base64 wire encoding, canonical JSON, and latency.
    pub async fn test_fetch(
        &self,
        extension_id: &str,
        query: &ExtensionQueryProto,
        http: &HttpClient,
        env: &EnvConfig,
    ) -> Result<ExtensionTestFetchResult> {
        let entry = {
            let guard = self.state.read().expect("ExtensionHub read lock");
            guard
                .extensions
                .get(extension_id.trim())
                .cloned()
                .ok_or_else(|| anyhow!("Extension '{}' not found", extension_id.trim()))?
        };

        let has_active_scope = MeterScope::current().is_some();
        let trigger = if has_active_scope && query.extra_params.contains_key("lazy_job_id") {
            match query
                .extra_params
                .get("lazy_trigger_kind")
                .map(|s| s.trim().to_ascii_lowercase())
                .as_deref()
            {
                Some("periodic_scan" | "periodicscan" | "scan") => InvocationTrigger::PeriodicScan,
                Some("admin_test" | "admintest" | "test_fetch") => InvocationTrigger::AdminTest,
                Some("dependency" | "dep") => InvocationTrigger::Dependency,
                _ => InvocationTrigger::LazyJob,
            }
        } else {
            InvocationTrigger::AdminTest
        };
        let parent_inv_id = crate::metering::current_invocation_id();
        let ctx = Self::invocation_ctx_for(&entry.manifest, trigger, parent_inv_id, Some(query));

        if has_active_scope {
            return if let Some(llm_spec) = manifest_to_active_llm_spec(&entry.manifest) {
                with_active_llm_spec(
                    llm_spec,
                    self.execute_test_fetch_inner(entry, query, http, env, &ctx, true),
                )
                .await
            } else {
                self.execute_test_fetch_inner(entry, query, http, env, &ctx, true)
                    .await
            };
        }

        let (res, event) = MeterScope::run(ctx.clone(), async {
            self.execute_test_fetch_inner(entry, query, http, env, &ctx, false)
                .await
        })
        .await;
        self.record_usage_event(&event).await;
        res
    }

    async fn execute_test_fetch_inner(
        &self,
        entry: RegisteredExtensionEntry,
        query: &ExtensionQueryProto,
        http: &HttpClient,
        env: &EnvConfig,
        ctx: &InvocationContext,
        caller_opened_scope: bool,
    ) -> Result<ExtensionTestFetchResult> {
        let mut auth_manifest = entry.manifest.clone();
        if caller_opened_scope && query.extra_params.contains_key("lazy_job_id") {
            // `LazyExecutor` already gated `requires_manual_approval && trigger != "approved"` before
            // dispatching `test_fetch`; prevent double-gating explicitly approved lazy jobs.
            auth_manifest.requires_manual_approval = false;
        }
        let decision = self.authorize_invocation(ctx, &auth_manifest).await;
        if !matches!(decision, InvocationDecision::Allow) {
            mark_denied();
            let (status, reason) = match decision {
                InvocationDecision::Deny { reason, .. } => ("denied", reason),
                InvocationDecision::RequireApproval { reason } => ("awaiting_approval", reason),
                InvocationDecision::Allow => ("allowed", String::new()),
            };
            let checked_at = now_iso();
            {
                let mut guard = self.state.write().expect("ExtensionHub write lock");
                if let Some(reg) = guard.extensions.get_mut(&entry.extension_id) {
                    reg.health_status = status.to_string();
                    reg.last_checked_at = Some(checked_at);
                }
            }
            return Ok(ExtensionTestFetchResult {
                extension_id: entry.extension_id.clone(),
                ok: false,
                health_status: status.to_string(),
                latency_ms: 0,
                envelope_count: 0,
                protobuf_byte_len: 0,
                protobuf_base64: String::new(),
                protobuf_wire_bytes: Vec::new(),
                canonical_json: json!({}),
                http_preview: json!({
                    "resolved": false,
                    "mode": "authorization_gate",
                    "reason": reason,
                }),
                error: Some(reason),
            });
        }

        let mut test_query = query.clone();
        let mut upstream_envs: Vec<ExtensionEnvelopeProto> = test_query.upstream_envelopes.clone();
        if has_declared_upstream(&entry.manifest) && upstream_envs.is_empty() {
            let upstream_entries: Vec<RegisteredExtensionEntry> = {
                let guard = self.state.read().expect("ExtensionHub read lock");
                let active: HashMap<String, RegisteredExtensionEntry> = guard
                    .extensions
                    .iter()
                    .filter(|(_, e)| {
                        if !e.manifest.enabled {
                            return false;
                        }
                        let prim = CapabilityDomain::try_from(e.manifest.domain)
                            .unwrap_or(CapabilityDomain::DomainUnspecified);
                        self.is_domain_module_enabled_with_map(&guard.modules, prim)
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                resolve_declared_upstream_ids(&entry.manifest, &active)
                    .into_iter()
                    .filter_map(|id| active.get(&id).cloned())
                    .collect()
            };
            let up_futs = upstream_entries.into_iter().map(|up| async move {
                let id = up.extension_id.clone();
                let up_ctx = Self::invocation_ctx_for(
                    &up.manifest,
                    InvocationTrigger::Dependency,
                    Some(ctx.invocation_id.clone().unwrap_or_default())
                        .filter(|s| !s.is_empty()),
                    Some(query),
                );
                let up_dec = self.authorize_invocation(&up_ctx, &up.manifest).await;
                if !matches!(up_dec, InvocationDecision::Allow) {
                    return (id, Ok(None));
                }
                let res = self
                    .execute_single_entry_internal(&up, query, &[], http, env, false)
                    .await;
                (id, res)
            });
            let mut ok_ids = HashSet::new();
            let mut env_map = HashMap::new();
            for (id, res) in join_all(up_futs).await {
                if let Ok(Some(envs)) = res {
                    ok_ids.insert(id.clone());
                    env_map.insert(id, envs);
                }
            }
            let up_ids: Vec<String> = ok_ids.iter().cloned().collect();
            let Some(collected) = collect_upstream_if_satisfied(
                &entry.manifest,
                &up_ids,
                &ok_ids,
                &env_map,
                &[],
            ) else {
                return Ok(ExtensionTestFetchResult {
                    extension_id: entry.extension_id.clone(),
                    ok: false,
                    health_status: "upstream_unavailable".to_string(),
                    latency_ms: 0,
                    envelope_count: 0,
                    protobuf_byte_len: 0,
                    protobuf_base64: String::new(),
                    protobuf_wire_bytes: Vec::new(),
                    canonical_json: json!({}),
                    http_preview: json!({
                        "resolved": false,
                        "mode": "dag_dependent_extractor",
                        "reason": "UPSTREAM_UNAVAILABLE: upstream dependencies failed, returned no usable envelopes, or are disabled",
                    }),
                    error: Some("UPSTREAM_UNAVAILABLE: required upstream extension output is unavailable".to_string()),
                });
            };
            upstream_envs = collected;
            test_query.upstream_envelopes = upstream_envs.clone();
        }

        let http_preview = if let Some(routing_preview) =
            crate::modules::routing_providers::preview_routing_extension_http(
                &entry.extension_id,
                &test_query,
                env,
            )
        {
            routing_preview
        } else {
            match entry.driver_slot.as_ref() {
                ExtensionDriverSlot::DeclarativeOrOpenApi(pkg) if pkg.has_live_http() => {
                    preview_http_resolution(pkg, &test_query, env)
                }
                _ => json!({ "resolved": false, "mode": "in_memory_catalog_or_native" }),
            }
        };

        let started = Instant::now();
        let exec_res = self
            .execute_single_entry_internal(&entry, &test_query, &upstream_envs, http, env, true)
            .await
            .map(|opt| opt.unwrap_or_default());
        let latency_ms = started.elapsed().as_millis() as u32;
        let checked_at = now_iso();

        let result = match exec_res {
            Ok(envelopes) => {
                let status = if envelopes.is_empty() {
                    "empty_for_scope"
                } else {
                    "healthy"
                };
                let bundle = aggregate_envelopes(query, &envelopes, None);
                let wire_bytes = if let Some(first) = envelopes.first() {
                    encode_proto_bytes(first)
                } else {
                    encode_proto_bytes(&bundle)
                };
                let b64 = if let Some(first) = envelopes.first() {
                    encode_proto_base64(first)
                } else {
                    encode_proto_base64(&bundle)
                };
                let display_envs: Vec<Value> =
                    envelopes.iter().map(envelope_to_display_json).collect();

                ExtensionTestFetchResult {
                    extension_id: entry.extension_id.clone(),
                    ok: true,
                    health_status: status.to_string(),
                    latency_ms,
                    envelope_count: envelopes.len(),
                    protobuf_byte_len: wire_bytes.len(),
                    protobuf_base64: b64,
                    protobuf_wire_bytes: wire_bytes,
                    canonical_json: json!({
                        "envelopes": display_envs,
                        "aggregatedBundle": serde_json::to_value(&bundle).unwrap_or_else(|_| json!({})),
                    }),
                    http_preview,
                    error: None,
                }
            }
            Err(err) => {
                let raw_msg = err.to_string();
                let status = if raw_msg.contains("AUTH_MISSING") {
                    "auth_missing"
                } else {
                    mark_failed();
                    "error"
                };
                let msg =
                    crate::modules::routing_providers::redact_routing_error(&raw_msg, &[]);
                ExtensionTestFetchResult {
                    extension_id: entry.extension_id.clone(),
                    ok: false,
                    health_status: status.to_string(),
                    latency_ms,
                    envelope_count: 0,
                    protobuf_byte_len: 0,
                    protobuf_base64: String::new(),
                    protobuf_wire_bytes: Vec::new(),
                    canonical_json: json!({}),
                    http_preview,
                    error: Some(msg),
                }
            }
        };

        {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            if let Some(reg) = guard.extensions.get_mut(&entry.extension_id) {
                reg.health_status = result.health_status.clone();
                reg.last_checked_at = Some(checked_at);
            }
        }

        Ok(result)
    }

    async fn execute_single_entry_internal(
        &self,
        entry: &RegisteredExtensionEntry,
        query: &ExtensionQueryProto,
        upstream: &[ExtensionEnvelopeProto],
        http: &HttpClient,
        env: &EnvConfig,
        is_test_fetch: bool,
    ) -> Result<Option<Vec<ExtensionEnvelopeProto>>> {
        let db_opt = self.resolve_hook_state().map(|s| s.db);
        let hub_arc = self
            .self_weak
            .read()
            .expect("ExtensionHub self_weak read")
            .upgrade();

        if crate::modules::routing_providers::RoutingProviderKind::from_id_or_alias(
            &entry.extension_id,
        )
        .is_some()
        {
            return crate::extensions::shared_cache::get_or_fetch(
                &self.shared_cache,
                db_opt.as_ref(),
                hub_arc.as_deref(),
                &entry.manifest,
                query,
                || async {
                    crate::modules::routing_providers::probe_routing_extension(
                        &entry.manifest,
                        query,
                        http,
                        env,
                    )
                    .await
                },
            )
            .await
            .map(Some);
        }
        match entry.driver_slot.as_ref() {
            ExtensionDriverSlot::DeclarativeOrOpenApi(pkg) => {
                let mut out = Vec::new();
                if pkg.has_catalog_rules() {
                    out.extend(execute_catalog_rules(pkg, &entry.manifest, query));
                }
                if pkg.has_live_http() {
                    if !is_test_fetch && is_optional_auth_missing(pkg, env) {
                        if out.is_empty() {
                            return Ok(None);
                        }
                        return Ok(Some(out));
                    }
                    let lim = self.limiter_for(&entry.manifest);
                    match crate::extensions::shared_cache::get_or_fetch(
                        &self.shared_cache,
                        db_opt.as_ref(),
                        hub_arc.as_deref(),
                        &entry.manifest,
                        query,
                        || async {
                            execute_live_http(
                                pkg,
                                &entry.manifest,
                                query,
                                http,
                                env,
                                &self.http_cache,
                                &lim,
                            )
                            .await
                        },
                    )
                    .await
                    {
                        Ok(live_envs) => out.extend(live_envs),
                        Err(e) => {
                            if out.is_empty() {
                                return Err(e);
                            }
                        }
                    }
                } else if has_declared_upstream(&entry.manifest) && !pkg.has_catalog_rules() {
                    let mut sub_q = query.clone();
                    if sub_q.upstream_envelopes.is_empty() && !upstream.is_empty() {
                        sub_q.upstream_envelopes = upstream.to_vec();
                    }
                    out.extend(execute_upstream_yaml_mapper(pkg, &entry.manifest, &sub_q)?);
                }
                Ok(Some(out))
            }
            ExtensionDriverSlot::Native(adapter) => crate::extensions::shared_cache::get_or_fetch(
                &self.shared_cache,
                db_opt.as_ref(),
                hub_arc.as_deref(),
                &entry.manifest,
                query,
                || async {
                    adapter
                        .execute_with_upstream(query, upstream, http, env)
                        .await
                },
            )
            .await
            .map(Some),
        }
    }

    fn is_domain_module_enabled_with_map(
        &self,
        modules: &BTreeMap<String, SystemModuleRow>,
        domain: CapabilityDomain,
    ) -> bool {
        let mod_id = match domain {
            CapabilityDomain::DomainGeocode => return true,
            CapabilityDomain::DomainPropertyFacts => "property_facts",
            CapabilityDomain::DomainValuation => "valuation",
            CapabilityDomain::DomainMarketTrends => "market_trends",
            CapabilityDomain::DomainElectricUtility
            | CapabilityDomain::DomainGasUtility
            | CapabilityDomain::DomainWaterSewerTrash
            | CapabilityDomain::DomainBroadband => "utilities_broadband",
            CapabilityDomain::DomainNaturalHazards => "natural_hazards",
            CapabilityDomain::DomainEnvironmental => "environmental_risk",
            CapabilityDomain::DomainLivability
            | CapabilityDomain::DomainSchools
            | CapabilityDomain::DomainCrime => "neighborhood_livability",
            CapabilityDomain::DomainHouse3d => "house_3d_lidar",
            CapabilityDomain::DomainListingsFeed => "property_search_shop",
            CapabilityDomain::DomainInsurance | CapabilityDomain::DomainTaxMandates => {
                "buyer_diligence"
            }
            CapabilityDomain::DomainPermits | CapabilityDomain::DomainPermitIntel => {
                "property_facts"
            }
            CapabilityDomain::DomainRouting => {
                let liv = modules
                    .get("neighborhood_livability")
                    .map(|m| m.enabled)
                    .unwrap_or(true);
                let com = modules
                    .get("commute_transit")
                    .map(|m| m.enabled)
                    .unwrap_or(true);
                return liv && com;
            }
            CapabilityDomain::DomainUnspecified => return true,
        };
        modules.get(mod_id).map(|m| m.enabled).unwrap_or(true)
    }

    /// Exports all currently registered extensions as `InstalledExtensionRow` records for DB persistence.
    pub fn export_installed_rows(&self) -> Vec<InstalledExtensionRow> {
        let guard = self.state.read().expect("ExtensionHub read lock");
        guard
            .extensions
            .values()
            .map(entry_to_db_row)
            .collect()
    }

    /// Reconciles `ExtensionHub` in-memory state from database `system_modules` and `installed_extensions` rows.
    pub fn reconcile_from_db_rows(
        &self,
        mod_rows: &[SystemModuleRow],
        ext_rows: &[InstalledExtensionRow],
    ) -> Result<Arc<RuntimeRegistrySnapshot>> {
        {
            let mut guard = self.state.write().expect("ExtensionHub write lock");
            for m in mod_rows {
                let mut row = m.clone();
                if row.module_id == "geocode" {
                    row.enabled = true;
                }
                guard.modules.insert(row.module_id.clone(), row);
            }

            let db_ext_ids: std::collections::HashSet<&str> = ext_rows
                .iter()
                .map(|r| r.extension_id.trim())
                .filter(|id| !id.is_empty())
                .collect();
            guard
                .extensions
                .retain(|id, e| e.manifest.builtin || db_ext_ids.contains(id.as_str()));

            for row in ext_rows {
                let clean_id = row.extension_id.trim();
                if clean_id.is_empty() {
                    continue;
                }
                if let Some(existing) = guard.extensions.get_mut(clean_id) {
                    if existing.manifest.builtin {
                        // Apply admin-saved runtime overrides onto the built-in package
                        existing.manifest.enabled = row.enabled;
                        existing.manifest.priority = row.priority.clamp(0, 10_000) as i32;
                        existing.config_json = row.config_json.clone();
                        if !row.health_status.is_empty() && row.health_status != "unknown" {
                            existing.health_status = row.health_status.clone();
                        }
                        existing.last_checked_at = row.last_checked_at.clone();
                        existing.updated_at = row.updated_at.clone();
                        continue;
                    }
                }

                // Custom admin-installed YAML/OpenAPI extension persisted in DB
                if !row.manifest_yaml.trim().is_empty() {
                    if let Ok(pkg) = parse_and_validate_package(
                        &row.manifest_yaml,
                        row.openapi_yaml.as_deref(),
                        Some(row.builtin),
                    ) {
                        let manifest = pkg.to_manifest_proto(
                            row.enabled,
                            row.priority.clamp(0, 10_000) as i32,
                        );
                        let manifest_yaml_owned = Some(pkg.manifest_yaml.clone());
                        let openapi_yaml_owned = pkg.openapi_yaml.clone();
                        guard.extensions.insert(
                            pkg.extension_id.clone(),
                            RegisteredExtensionEntry {
                                extension_id: pkg.extension_id.clone(),
                                manifest,
                                driver_slot: Arc::new(
                                    ExtensionDriverSlot::DeclarativeOrOpenApi(Arc::new(pkg)),
                                ),
                                manifest_yaml: manifest_yaml_owned,
                                openapi_yaml: openapi_yaml_owned,
                                config_json: row.config_json.clone(),
                                health_status: row.health_status.clone(),
                                last_checked_at: row.last_checked_at.clone(),
                                installed_at: row.installed_at.clone(),
                                updated_at: row.updated_at.clone(),
                            },
                        );
                    }
                }
            }
            guard.cached_snapshot = None;
        }
        Ok(self.bump_and_refresh())
    }

    /// Full bi-directional synchronization with SQLite / MySQL `DbStore`:
    /// 1. Seeds `DEFAULT_SYSTEM_MODULES` if empty.
    /// 2. Upserts any newly added built-in `extensions/*/manifest.yaml` packages into `installed_extensions`
    ///    without overwriting existing admin `enabled`/`priority` overrides.
    /// 3. Applies persisted DB module & extension state into `ExtensionHub` and returns the fresh snapshot.
    pub async fn sync_with_db(&self, db: &DbStore) -> Result<Arc<RuntimeRegistrySnapshot>> {
        db.seed_default_system_modules_if_empty().await?;
        let existing_exts = db.list_installed_extensions().await?;
        let existing_ids: std::collections::HashSet<String> = existing_exts
            .iter()
            .map(|r| r.extension_id.clone())
            .collect();

        for builtin_row in self.export_installed_rows() {
            if !existing_ids.contains(&builtin_row.extension_id) {
                db.upsert_installed_extension(&builtin_row).await?;
            }
        }

        let mod_rows = db.list_system_modules().await?;
        let refreshed_exts = db.list_installed_extensions().await?;
        self.reconcile_from_db_rows(&mod_rows, &refreshed_exts)
    }
}

fn seed_default_modules_map() -> BTreeMap<String, SystemModuleRow> {
    let now = now_iso();
    let mut map = BTreeMap::new();
    for &(id, cat, name, desc, prio) in DEFAULT_SYSTEM_MODULES {
        map.insert(
            id.to_string(),
            SystemModuleRow {
                module_id: id.to_string(),
                category: cat.to_string(),
                display_name: name.to_string(),
                description_text: desc.to_string(),
                enabled: true,
                priority: prio,
                config_json: "{}".to_string(),
                updated_at: now.clone(),
            },
        );
    }
    map
}

fn build_snapshot_from_state(state: &HubInnerState, gen_v: u64) -> RuntimeRegistrySnapshot {
    let modules: HashMap<String, SystemModuleRow> = state
        .modules
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut manifests: Vec<ExtensionManifestProto> = state
        .extensions
        .values()
        .map(|e| e.manifest.clone())
        .collect();
    manifests.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.extension_id.cmp(&b.extension_id))
    });

    let mut by_domain: HashMap<i32, Vec<String>> = HashMap::new();
    for m in &manifests {
        if !m.enabled {
            continue;
        }
        by_domain
            .entry(m.domain)
            .or_default()
            .push(m.extension_id.clone());
        for &ad in &m.additional_domains {
            let list = by_domain.entry(ad).or_default();
            if !list.contains(&m.extension_id) {
                list.push(m.extension_id.clone());
            }
        }
    }

    RuntimeRegistrySnapshot {
        generation_version: gen_v,
        modules,
        enabled_extension_ids_by_domain: by_domain,
        manifests,
    }
}

fn entry_to_db_row(entry: &RegisteredExtensionEntry) -> InstalledExtensionRow {
    let domain = CapabilityDomain::try_from(entry.manifest.domain)
        .unwrap_or(CapabilityDomain::DomainUnspecified);
    let driver =
        DriverKind::try_from(entry.manifest.driver).unwrap_or(DriverKind::DriverDeclarativeYaml);
    let (manifest_yaml, openapi_yaml) = match entry.driver_slot.as_ref() {
        ExtensionDriverSlot::DeclarativeOrOpenApi(pkg) => {
            (pkg.manifest_yaml.clone(), pkg.openapi_yaml.clone())
        }
        ExtensionDriverSlot::Native(_) => (String::new(), None),
    };

    InstalledExtensionRow {
        extension_id: entry.extension_id.clone(),
        name: entry.manifest.name.clone(),
        version: entry.manifest.version.clone(),
        domain: capability_domain_slug(domain).to_string(),
        driver_kind: driver_kind_slug(driver).to_string(),
        enabled: entry.manifest.enabled,
        builtin: entry.manifest.builtin,
        priority: entry.manifest.priority as i64,
        manifest_yaml,
        openapi_yaml,
        config_json: entry.config_json.clone(),
        proto_message_type: entry.manifest.proto_message_type.clone(),
        health_status: entry.health_status.clone(),
        last_checked_at: entry.last_checked_at.clone(),
        installed_at: entry.installed_at.clone(),
        updated_at: entry.updated_at.clone(),
    }
}

struct PlannedEntry {
    entry: RegisteredExtensionEntry,
    upstream_ids: Vec<String>,
}

struct ExecutionWavePlan {
    waves: Vec<Vec<PlannedEntry>>,
}

fn has_declared_upstream(manifest: &ExtensionManifestProto) -> bool {
    !manifest.depends_on.is_empty() || !manifest.input_from_domains.is_empty()
}

fn entry_matches_domains(
    entry: &RegisteredExtensionEntry,
    domains: Option<&[CapabilityDomain]>,
) -> bool {
    let Some(wanted) = domains else {
        return true;
    };
    let prim = CapabilityDomain::try_from(entry.manifest.domain)
        .unwrap_or(CapabilityDomain::DomainUnspecified);
    wanted.contains(&prim)
        || entry.manifest.additional_domains.iter().any(|&ad| {
            CapabilityDomain::try_from(ad)
                .map(|d| wanted.contains(&d))
                .unwrap_or(false)
        })
}

fn filter_envelopes_for_domains(
    envelopes: &[ExtensionEnvelopeProto],
    domains: Option<&[CapabilityDomain]>,
) -> Vec<ExtensionEnvelopeProto> {
    let Some(wanted) = domains else {
        return envelopes.to_vec();
    };
    envelopes
        .iter()
        .filter(|e| {
            CapabilityDomain::try_from(e.domain)
                .map(|d| wanted.contains(&d))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn resolve_declared_upstream_ids(
    manifest: &ExtensionManifestProto,
    available: &HashMap<String, RegisteredExtensionEntry>,
) -> Vec<String> {
    let mut out = Vec::new();
    for dep in &manifest.depends_on {
        if dep != &manifest.extension_id
            && available.contains_key(dep)
            && !out.contains(dep)
        {
            out.push(dep.clone());
        }
    }
    for &dom_i32 in &manifest.input_from_domains {
        for (id, e) in available {
            if id != &manifest.extension_id
                && (e.manifest.domain == dom_i32 || e.manifest.additional_domains.contains(&dom_i32))
                && !out.contains(id)
            {
                out.push(id.clone());
            }
        }
    }
    out
}

fn collect_upstream_if_satisfied(
    manifest: &ExtensionManifestProto,
    active_upstream_ids: &[String],
    succeeded_ids: &HashSet<String>,
    envelopes_by_id: &HashMap<String, Vec<ExtensionEnvelopeProto>>,
    caller_upstream: &[ExtensionEnvelopeProto],
) -> Option<Vec<ExtensionEnvelopeProto>> {
    if !has_declared_upstream(manifest) {
        return Some(caller_upstream.to_vec());
    }
    if !caller_upstream.is_empty() && active_upstream_ids.is_empty() {
        return Some(caller_upstream.to_vec());
    }
    let require_all = manifest.upstream_require_mode.eq_ignore_ascii_case("all");
    if require_all {
        for dep_id in &manifest.depends_on {
            if !succeeded_ids.contains(dep_id) {
                return None;
            }
        }
    }
    let mut collected = Vec::new();
    let mut any_upstream_succeeded = false;
    for up_id in active_upstream_ids {
        if succeeded_ids.contains(up_id) {
            any_upstream_succeeded = true;
            if let Some(envs) = envelopes_by_id.get(up_id) {
                collected.extend(envs.clone());
            }
        }
    }
    if !caller_upstream.is_empty() {
        collected.extend(caller_upstream.to_vec());
        any_upstream_succeeded = true;
    }
    if !any_upstream_succeeded || (collected.is_empty() && !require_all) {
        return None;
    }
    Some(collected)
}

fn apply_wave_trace_metadata(
    bundle: &mut AggregatedPropertyBundleProto,
    max_wave_by_domain: &HashMap<i32, u32>,
    skipped_by_domain: &HashMap<i32, Vec<String>>,
) {
    for tr in &mut bundle.resolution_traces {
        if let Some(&w) = max_wave_by_domain.get(&tr.domain) {
            tr.wave_index = w;
        }
        if let Some(sk) = skipped_by_domain.get(&tr.domain) {
            for s in sk {
                if !tr.skipped_extension_ids.contains(s) {
                    tr.skipped_extension_ids.push(s.clone());
                }
            }
        }
    }
}

fn detect_dag_cycle_for_candidate(
    existing: &BTreeMap<String, RegisteredExtensionEntry>,
    candidate: &ExtensionManifestProto,
) -> Option<Vec<String>> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_manifests: Vec<&ExtensionManifestProto> = existing
        .values()
        .filter(|e| e.extension_id != candidate.extension_id)
        .map(|e| &e.manifest)
        .collect();
    all_manifests.push(candidate);

    for m in &all_manifests {
        let mut deps = Vec::new();
        for d in &m.depends_on {
            if !deps.contains(d) {
                deps.push(d.clone());
            }
        }
        for &dom_i32 in &m.input_from_domains {
            for other in &all_manifests {
                if other.extension_id != m.extension_id
                    && (other.domain == dom_i32 || other.additional_domains.contains(&dom_i32))
                    && !deps.contains(&other.extension_id)
                {
                    deps.push(other.extension_id.clone());
                }
            }
        }
        graph.insert(m.extension_id.clone(), deps);
    }

    fn dfs(
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        state: &mut HashMap<String, u8>,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        state.insert(node.to_string(), 1);
        stack.push(node.to_string());
        if let Some(deps) = graph.get(node) {
            for dep in deps {
                match state.get(dep).copied().unwrap_or(0) {
                    1 => {
                        let start = stack.iter().position(|s| s == dep).unwrap_or(0);
                        let mut cycle = stack[start..].to_vec();
                        cycle.push(dep.clone());
                        return Some(cycle);
                    }
                    0 => {
                        if let Some(c) = dfs(dep, graph, state, stack) {
                            return Some(c);
                        }
                    }
                    _ => {}
                }
            }
        }
        stack.pop();
        state.insert(node.to_string(), 2);
        None
    }

    let mut visit_state: HashMap<String, u8> = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    dfs(&candidate.extension_id, &graph, &mut visit_state, &mut stack)
}

fn entry_to_admin_json(
    entry: &RegisteredExtensionEntry,
    env: Option<&EnvConfig>,
    include_sources: bool,
) -> Value {
    let m = &entry.manifest;
    let domain =
        CapabilityDomain::try_from(m.domain).unwrap_or(CapabilityDomain::DomainUnspecified);
    let driver = DriverKind::try_from(m.driver).unwrap_or(DriverKind::DriverDeclarativeYaml);
    let prov = ProvenanceTier::try_from(m.default_provenance)
        .unwrap_or(ProvenanceTier::ProvenanceUnspecified);

    let (auth_type, auth_env_var, auth_configured) = if let Some(routing_conf) =
        crate::modules::routing_providers::is_routing_extension_configured(&m.extension_id, env)
    {
        let a_type = m
            .auth
            .as_ref()
            .map(|a| a.auth_type.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("none")
            .to_string();
        let a_env = m
            .auth
            .as_ref()
            .map(|a| a.env_var.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        (a_type, a_env, routing_conf)
    } else {
        match m.auth.as_ref() {
            Some(a) if !a.auth_type.is_empty() && a.auth_type != "none" => {
                let configured = env
                    .and_then(|cfg| cfg.get(&a.env_var))
                    .map(|v| !v.trim().is_empty())
                    .or_else(|| {
                        std::env::var(&a.env_var)
                            .ok()
                            .map(|v| !v.trim().is_empty())
                    })
                    .unwrap_or(false);
                (a.auth_type.clone(), Some(a.env_var.clone()), configured)
            }
            _ => ("none".to_string(), None, true),
        }
    };

    let tier = manifest_tier(m);
    let license_class = manifest_license_class(m);
    let execution_mode = manifest_execution_mode(m);
    let requires_manual_approval = m.requires_manual_approval;
    let min_plan_tier = m.min_plan_tier.trim();
    let metering_dims = manifest_metering_dimensions(m);
    let section_id = manifest_section_id(m);

    let mut obj = json!({
        "extensionId": m.extension_id,
        "name": m.name,
        "version": m.version,
        "description": m.description,
        "domain": capability_domain_slug(domain),
        "domainEnum": domain.as_str_name(),
        "additionalDomains": m.additional_domains.iter().filter_map(|&d| CapabilityDomain::try_from(d).ok()).map(capability_domain_slug).collect::<Vec<_>>(),
        "driverKind": driver_kind_slug(driver),
        "driverLabel": driver_kind_label(driver),
        "tier": tier,
        "licenseClass": license_class,
        "executionMode": execution_mode,
        "requiresManualApproval": requires_manual_approval,
        "minPlanTier": min_plan_tier,
        "meteringDimensions": metering_dims,
        "sectionId": section_id,
        "enabled": m.enabled,
        "builtin": m.builtin,
        "priority": m.priority,
        "cacheTtlSecs": m.cache_ttl_secs,
        "protoMessageType": m.proto_message_type,
        "defaultProvenance": provenance_tier_slug(prov),
        "homepageUrl": m.homepage_url,
        "documentationUrl": m.documentation_url,
        "sourceAttribution": m.source_attribution,
        "codeowners": m.codeowners,
        "geoScope": m.geo_scope,
        "authType": auth_type,
        "authEnvVar": auth_env_var,
        "authConfigured": auth_configured,
        "healthStatus": entry.health_status,
        "lastCheckedAt": entry.last_checked_at,
        "installedAt": entry.installed_at,
        "updatedAt": entry.updated_at,
        "dependsOn": m.depends_on,
        "inputFromDomains": m.input_from_domains.iter().filter_map(|&d| CapabilityDomain::try_from(d).ok()).map(capability_domain_slug).collect::<Vec<_>>(),
        "upstreamRequireMode": if m.upstream_require_mode.is_empty() { "any" } else { m.upstream_require_mode.as_str() },
    });

    if include_sources {
        obj["manifestYaml"] = match entry.manifest_yaml.as_ref() {
            Some(y) => Value::String(y.clone()),
            None => Value::Null,
        };
        obj["openapiYaml"] = match entry.openapi_yaml.as_ref() {
            Some(oas) => Value::String(oas.clone()),
            None => Value::Null,
        };
        obj["configJson"] = Value::String(entry.config_json.clone());
    }

    obj
}
