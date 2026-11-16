//! Lossless `From` / `TryFrom` conversions between `open_estate::metering` domain types
//! and generated Protocol Buffers (`openestate.extension.v1` in `metering.proto` & `extension.proto`).

use anyhow::{anyhow, Error};

use crate::extensions::proto::{
    self, invocation_decision_proto, parse_reasoning_effort, reasoning_effort_slug,
};
use crate::metering::{
    default_version, ActiveLlmSpec, ActiveLlmTaskSpec, ExtensionTier, ExtensionUsageEvent,
    InvocationContext, InvocationDecision, InvocationOutcome, InvocationTrigger, LlmUsageRecord,
    UsageDimensions,
};

fn trim_non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn trim_opt_non_empty(raw: Option<&str>) -> Option<String> {
    raw.and_then(trim_non_empty)
}

fn proto_tier_from_i32(raw: i32) -> Result<ExtensionTier, Error> {
    match proto::ExtensionTier::try_from(raw) {
        Ok(proto::ExtensionTier::Unspecified | proto::ExtensionTier::Basic) => {
            Ok(ExtensionTier::Basic)
        }
        Ok(proto::ExtensionTier::Premium) => Ok(ExtensionTier::Premium),
        Err(_) => Err(anyhow!("unknown ExtensionTier enum value: {raw}")),
    }
}

fn proto_trigger_from_i32(raw: i32) -> Result<InvocationTrigger, Error> {
    match proto::InvocationTrigger::try_from(raw) {
        Ok(proto::InvocationTrigger::Unspecified | proto::InvocationTrigger::UserRequest) => {
            Ok(InvocationTrigger::UserRequest)
        }
        Ok(proto::InvocationTrigger::PeriodicScan) => Ok(InvocationTrigger::PeriodicScan),
        Ok(proto::InvocationTrigger::LazyJob) => Ok(InvocationTrigger::LazyJob),
        Ok(proto::InvocationTrigger::AdminTest) => Ok(InvocationTrigger::AdminTest),
        Ok(proto::InvocationTrigger::Dependency) => Ok(InvocationTrigger::Dependency),
        Err(_) => Err(anyhow!("unknown InvocationTrigger enum value: {raw}")),
    }
}

fn proto_outcome_from_i32(raw: i32) -> Result<InvocationOutcome, Error> {
    match proto::InvocationOutcome::try_from(raw) {
        Ok(proto::InvocationOutcome::Unspecified | proto::InvocationOutcome::Ok) => {
            Ok(InvocationOutcome::Ok)
        }
        Ok(proto::InvocationOutcome::Failed) => Ok(InvocationOutcome::Failed),
        Ok(proto::InvocationOutcome::Cached) => Ok(InvocationOutcome::Cached),
        Ok(proto::InvocationOutcome::Denied) => Ok(InvocationOutcome::Denied),
        Err(_) => Err(anyhow!("unknown InvocationOutcome enum value: {raw}")),
    }
}

fn reasoning_effort_from_opt(raw: Option<&str>) -> i32 {
    raw.and_then(parse_reasoning_effort)
        .unwrap_or(proto::ReasoningEffort::Unspecified) as i32
}

fn reasoning_effort_opt_from_i32(raw: i32) -> Option<String> {
    proto::ReasoningEffort::try_from(raw)
        .ok()
        .filter(|e| *e != proto::ReasoningEffort::Unspecified)
        .map(|e| reasoning_effort_slug(e).to_string())
}

impl From<ExtensionTier> for proto::ExtensionTier {
    fn from(value: ExtensionTier) -> Self {
        match value {
            ExtensionTier::Basic => Self::Basic,
            ExtensionTier::Premium => Self::Premium,
        }
    }
}

impl TryFrom<proto::ExtensionTier> for ExtensionTier {
    type Error = Error;

    fn try_from(value: proto::ExtensionTier) -> Result<Self, Self::Error> {
        match value {
            proto::ExtensionTier::Basic => Ok(Self::Basic),
            proto::ExtensionTier::Premium => Ok(Self::Premium),
            proto::ExtensionTier::Unspecified => Err(anyhow!(
                "ExtensionTier::Unspecified cannot be mapped strictly"
            )),
        }
    }
}

impl From<InvocationTrigger> for proto::InvocationTrigger {
    fn from(value: InvocationTrigger) -> Self {
        match value {
            InvocationTrigger::UserRequest => Self::UserRequest,
            InvocationTrigger::PeriodicScan => Self::PeriodicScan,
            InvocationTrigger::LazyJob => Self::LazyJob,
            InvocationTrigger::AdminTest => Self::AdminTest,
            InvocationTrigger::Dependency => Self::Dependency,
        }
    }
}

impl TryFrom<proto::InvocationTrigger> for InvocationTrigger {
    type Error = Error;

    fn try_from(value: proto::InvocationTrigger) -> Result<Self, Self::Error> {
        match value {
            proto::InvocationTrigger::UserRequest => Ok(Self::UserRequest),
            proto::InvocationTrigger::PeriodicScan => Ok(Self::PeriodicScan),
            proto::InvocationTrigger::LazyJob => Ok(Self::LazyJob),
            proto::InvocationTrigger::AdminTest => Ok(Self::AdminTest),
            proto::InvocationTrigger::Dependency => Ok(Self::Dependency),
            proto::InvocationTrigger::Unspecified => Err(anyhow!(
                "InvocationTrigger::Unspecified cannot be mapped strictly"
            )),
        }
    }
}

impl From<InvocationOutcome> for proto::InvocationOutcome {
    fn from(value: InvocationOutcome) -> Self {
        match value {
            InvocationOutcome::Ok => Self::Ok,
            InvocationOutcome::Failed => Self::Failed,
            InvocationOutcome::Cached => Self::Cached,
            InvocationOutcome::Denied => Self::Denied,
        }
    }
}

impl TryFrom<proto::InvocationOutcome> for InvocationOutcome {
    type Error = Error;

    fn try_from(value: proto::InvocationOutcome) -> Result<Self, Self::Error> {
        match value {
            proto::InvocationOutcome::Ok => Ok(Self::Ok),
            proto::InvocationOutcome::Failed => Ok(Self::Failed),
            proto::InvocationOutcome::Cached => Ok(Self::Cached),
            proto::InvocationOutcome::Denied => Ok(Self::Denied),
            proto::InvocationOutcome::Unspecified => Err(anyhow!(
                "InvocationOutcome::Unspecified cannot be mapped strictly"
            )),
        }
    }
}

impl From<&UsageDimensions> for proto::UsageDimensionsProto {
    fn from(d: &UsageDimensions) -> Self {
        Self {
            invocations: d.invocations,
            invocations_ok: d.invocations_ok,
            invocations_failed: d.invocations_failed,
            invocations_cached: d.invocations_cached,
            invocations_denied: d.invocations_denied,
            wall_time_ms: d.wall_time_ms,
            llm_input_tokens: d.llm_input_tokens,
            llm_cached_input_tokens: d.llm_cached_input_tokens,
            llm_cache_write_tokens: d.llm_cache_write_tokens,
            llm_output_tokens: d.llm_output_tokens,
            llm_reasoning_tokens: d.llm_reasoning_tokens,
            llm_cost_micros: d.llm_cost_micros,
            browser_session_ms: d.browser_session_ms,
            browser_page_loads: d.browser_page_loads,
            emails_sent: d.emails_sent,
            emails_ingested: d.emails_ingested,
            documents_ingested: d.documents_ingested,
            pages_ocr: d.pages_ocr,
            bytes_downloaded: d.bytes_downloaded,
            bytes_stored: d.bytes_stored,
            external_api_calls: d.external_api_calls,
            external_api_cost_micros: d.external_api_cost_micros,
        }
    }
}

impl From<UsageDimensions> for proto::UsageDimensionsProto {
    fn from(d: UsageDimensions) -> Self {
        Self::from(&d)
    }
}

impl From<&proto::UsageDimensionsProto> for UsageDimensions {
    fn from(p: &proto::UsageDimensionsProto) -> Self {
        Self {
            invocations: p.invocations,
            invocations_ok: p.invocations_ok,
            invocations_failed: p.invocations_failed,
            invocations_cached: p.invocations_cached,
            invocations_denied: p.invocations_denied,
            wall_time_ms: p.wall_time_ms,
            llm_input_tokens: p.llm_input_tokens,
            llm_cached_input_tokens: p.llm_cached_input_tokens,
            llm_cache_write_tokens: p.llm_cache_write_tokens,
            llm_output_tokens: p.llm_output_tokens,
            llm_reasoning_tokens: p.llm_reasoning_tokens,
            llm_cost_micros: p.llm_cost_micros,
            browser_session_ms: p.browser_session_ms,
            browser_page_loads: p.browser_page_loads,
            emails_sent: p.emails_sent,
            emails_ingested: p.emails_ingested,
            documents_ingested: p.documents_ingested,
            pages_ocr: p.pages_ocr,
            bytes_downloaded: p.bytes_downloaded,
            bytes_stored: p.bytes_stored,
            external_api_calls: p.external_api_calls,
            external_api_cost_micros: p.external_api_cost_micros,
        }
    }
}

impl From<proto::UsageDimensionsProto> for UsageDimensions {
    fn from(p: proto::UsageDimensionsProto) -> Self {
        Self::from(&p)
    }
}

impl From<&LlmUsageRecord> for proto::LlmUsageRecordProto {
    fn from(r: &LlmUsageRecord) -> Self {
        Self {
            model: r.model.clone(),
            effort: r.effort.clone(),
            profile: r.profile.clone(),
            task: r.task.clone(),
            input_tokens: r.input_tokens,
            cached_input_tokens: r.cached_input_tokens,
            cache_write_tokens: r.cache_write_tokens,
            output_tokens: r.output_tokens,
            reasoning_tokens: r.reasoning_tokens,
            cost_micros: r.cost_micros,
        }
    }
}

impl From<LlmUsageRecord> for proto::LlmUsageRecordProto {
    fn from(r: LlmUsageRecord) -> Self {
        Self::from(&r)
    }
}

impl From<&proto::LlmUsageRecordProto> for LlmUsageRecord {
    fn from(p: &proto::LlmUsageRecordProto) -> Self {
        Self {
            model: p.model.clone(),
            effort: p.effort.clone(),
            profile: p.profile.clone(),
            task: p.task.clone(),
            input_tokens: p.input_tokens,
            cached_input_tokens: p.cached_input_tokens,
            cache_write_tokens: p.cache_write_tokens,
            output_tokens: p.output_tokens,
            reasoning_tokens: p.reasoning_tokens,
            cost_micros: p.cost_micros,
        }
    }
}

impl From<proto::LlmUsageRecordProto> for LlmUsageRecord {
    fn from(p: proto::LlmUsageRecordProto) -> Self {
        Self::from(&p)
    }
}

impl From<&ActiveLlmTaskSpec> for proto::LlmTaskSpecProto {
    fn from(t: &ActiveLlmTaskSpec) -> Self {
        Self {
            task: t.task.clone(),
            profile: t.profile.clone().unwrap_or_default(),
            model: t.model.clone(),
            effort: reasoning_effort_from_opt(t.effort.as_deref()),
            max_output_tokens: t.max_output_tokens,
        }
    }
}

impl From<ActiveLlmTaskSpec> for proto::LlmTaskSpecProto {
    fn from(t: ActiveLlmTaskSpec) -> Self {
        Self::from(&t)
    }
}

impl From<&proto::LlmTaskSpecProto> for ActiveLlmTaskSpec {
    fn from(p: &proto::LlmTaskSpecProto) -> Self {
        Self {
            task: p.task.trim().to_string(),
            profile: trim_non_empty(&p.profile),
            model: trim_opt_non_empty(p.model.as_deref()),
            effort: reasoning_effort_opt_from_i32(p.effort),
            max_output_tokens: p.max_output_tokens,
        }
    }
}

impl From<proto::LlmTaskSpecProto> for ActiveLlmTaskSpec {
    fn from(p: proto::LlmTaskSpecProto) -> Self {
        Self::from(&p)
    }
}

impl From<&ActiveLlmSpec> for proto::LlmSpecProto {
    fn from(s: &ActiveLlmSpec) -> Self {
        Self {
            profile: s.profile.clone().unwrap_or_default(),
            model: s.model.clone(),
            effort: reasoning_effort_from_opt(s.effort.as_deref()),
            max_output_tokens: s.max_output_tokens,
            tasks: s.tasks.iter().map(proto::LlmTaskSpecProto::from).collect(),
        }
    }
}

impl From<ActiveLlmSpec> for proto::LlmSpecProto {
    fn from(s: ActiveLlmSpec) -> Self {
        Self::from(&s)
    }
}

impl From<&proto::LlmSpecProto> for ActiveLlmSpec {
    fn from(p: &proto::LlmSpecProto) -> Self {
        Self {
            extension_id: String::new(),
            profile: trim_non_empty(&p.profile),
            model: trim_opt_non_empty(p.model.as_deref()),
            effort: reasoning_effort_opt_from_i32(p.effort),
            max_output_tokens: p.max_output_tokens,
            tasks: p.tasks.iter().map(ActiveLlmTaskSpec::from).collect(),
        }
    }
}

impl From<proto::LlmSpecProto> for ActiveLlmSpec {
    fn from(p: proto::LlmSpecProto) -> Self {
        Self::from(&p)
    }
}

impl From<&InvocationContext> for proto::InvocationContextProto {
    fn from(ctx: &InvocationContext) -> Self {
        Self {
            invocation_id: ctx.invocation_id.clone(),
            parent_invocation_id: ctx.parent_invocation_id.clone(),
            extension_id: ctx.extension_id.clone(),
            version: ctx.version.clone(),
            tier: proto::ExtensionTier::from(ctx.tier) as i32,
            trigger: proto::InvocationTrigger::from(ctx.trigger) as i32,
            actor_user_id: ctx.actor_user_id.clone(),
            org_id: ctx.org_id.clone(),
            slug: ctx.slug.clone(),
            billable: ctx.billable,
            llm_spec: ctx.llm_spec.as_ref().map(proto::LlmSpecProto::from),
        }
    }
}

impl From<InvocationContext> for proto::InvocationContextProto {
    fn from(ctx: InvocationContext) -> Self {
        Self::from(&ctx)
    }
}

impl TryFrom<proto::InvocationContextProto> for InvocationContext {
    type Error = Error;

    fn try_from(p: proto::InvocationContextProto) -> Result<Self, Self::Error> {
        let tier = proto_tier_from_i32(p.tier)?;
        let trigger = proto_trigger_from_i32(p.trigger)?;
        let version = if p.version.trim().is_empty() {
            default_version()
        } else {
            p.version
        };
        let llm_spec = p.llm_spec.map(|spec| {
            let mut active = ActiveLlmSpec::from(spec);
            if active.extension_id.trim().is_empty() {
                active.extension_id = p.extension_id.clone();
            }
            active
        });
        Ok(Self {
            invocation_id: p.invocation_id,
            parent_invocation_id: p.parent_invocation_id,
            extension_id: p.extension_id,
            version,
            tier,
            trigger,
            actor_user_id: p.actor_user_id,
            org_id: p.org_id,
            slug: p.slug,
            billable: p.billable,
            llm_spec,
        })
    }
}

impl From<&ExtensionUsageEvent> for proto::ExtensionUsageEventProto {
    fn from(ev: &ExtensionUsageEvent) -> Self {
        Self {
            invocation_id: ev.invocation_id.clone(),
            parent_invocation_id: ev.parent_invocation_id.clone(),
            extension_id: ev.extension_id.clone(),
            version: ev.version.clone(),
            tier: proto::ExtensionTier::from(ev.tier) as i32,
            trigger: proto::InvocationTrigger::from(ev.trigger) as i32,
            actor_user_id: ev.actor_user_id.clone(),
            org_id: ev.org_id.clone(),
            slug: ev.slug.clone(),
            started_at: ev.started_at.clone(),
            finished_at: ev.finished_at.clone(),
            outcome: proto::InvocationOutcome::from(ev.outcome) as i32,
            dims: Some(proto::UsageDimensionsProto::from(&ev.dims)),
            billable: ev.billable,
            llm_model: ev.llm_model.clone(),
            llm_effort: ev.llm_effort.clone(),
            llm_calls: ev
                .llm_calls
                .iter()
                .map(proto::LlmUsageRecordProto::from)
                .collect(),
        }
    }
}

impl From<ExtensionUsageEvent> for proto::ExtensionUsageEventProto {
    fn from(ev: ExtensionUsageEvent) -> Self {
        Self::from(&ev)
    }
}

impl TryFrom<proto::ExtensionUsageEventProto> for ExtensionUsageEvent {
    type Error = Error;

    fn try_from(p: proto::ExtensionUsageEventProto) -> Result<Self, Self::Error> {
        let tier = proto_tier_from_i32(p.tier)?;
        let trigger = proto_trigger_from_i32(p.trigger)?;
        let outcome = proto_outcome_from_i32(p.outcome)?;
        Ok(Self {
            invocation_id: p.invocation_id,
            parent_invocation_id: p.parent_invocation_id,
            extension_id: p.extension_id,
            version: p.version,
            tier,
            trigger,
            actor_user_id: p.actor_user_id,
            org_id: p.org_id,
            slug: p.slug,
            started_at: p.started_at,
            finished_at: p.finished_at,
            outcome,
            dims: p.dims.map(UsageDimensions::from).unwrap_or_default(),
            billable: p.billable,
            llm_model: p.llm_model,
            llm_effort: p.llm_effort,
            llm_calls: p.llm_calls.into_iter().map(LlmUsageRecord::from).collect(),
        })
    }
}

impl From<&InvocationDecision> for proto::InvocationDecisionProto {
    fn from(d: &InvocationDecision) -> Self {
        let decision = match d {
            InvocationDecision::Allow => {
                invocation_decision_proto::Decision::Allow(proto::InvocationAllowProto {})
            }
            InvocationDecision::Deny {
                reason,
                required_tier,
            } => invocation_decision_proto::Decision::Deny(proto::InvocationDenyProto {
                reason: reason.clone(),
                required_tier: required_tier.clone(),
            }),
            InvocationDecision::RequireApproval { reason } => {
                invocation_decision_proto::Decision::RequireApproval(
                    proto::InvocationRequireApprovalProto {
                        reason: reason.clone(),
                    },
                )
            }
        };
        Self {
            decision: Some(decision),
        }
    }
}

impl From<InvocationDecision> for proto::InvocationDecisionProto {
    fn from(d: InvocationDecision) -> Self {
        Self::from(&d)
    }
}

impl TryFrom<proto::InvocationDecisionProto> for InvocationDecision {
    type Error = Error;

    fn try_from(p: proto::InvocationDecisionProto) -> Result<Self, Self::Error> {
        match p.decision {
            Some(invocation_decision_proto::Decision::Allow(_)) => Ok(Self::Allow),
            Some(invocation_decision_proto::Decision::Deny(d)) => Ok(Self::Deny {
                reason: d.reason,
                required_tier: d.required_tier,
            }),
            Some(invocation_decision_proto::Decision::RequireApproval(r)) => {
                Ok(Self::RequireApproval { reason: r.reason })
            }
            None => Err(anyhow!("InvocationDecisionProto.decision is missing")),
        }
    }
}
