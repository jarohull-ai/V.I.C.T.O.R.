//! Minimalny przepływ wykonania narzędzia:
//! registry/schema -> policy -> approval -> executor -> context + audit.

use std::sync::Arc;

use serde_json::to_string;
use victor_context::{
    ContextContent, ContextItem, ContextItemId, ContextItemKind, ContextScope, ContextSource,
    ContextStore, MemoryTier, Retention, Sensitivity,
};
use victor_domain::{Approval, Plan, UnixMs};
use victor_policy::{Outcome, PolicyEngine};
use victor_tools::{
    AuditOutcome, AuditSink, ResultTier, ToolAuditEvent, ToolError, ToolExecutor, ToolRegistry,
    ToolResult, ToolResultStore,
};

use crate::error::RuntimeError;

#[derive(Debug, thiserror::Error)]
pub enum ToolRuntimeError {
    #[error(transparent)]
    Policy(#[from] victor_policy::PolicyError),
    #[error("narzędzie odrzucone przez politykę: {0}")]
    PolicyDenied(String),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Context(#[from] victor_context::ContextError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

pub struct ToolRuntime {
    registry: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    context: Arc<dyn ContextStore>,
    audit: Arc<dyn AuditSink>,
    transactional_store: Option<Arc<dyn ToolResultStore>>,
}

impl ToolRuntime {
    pub fn new(
        registry: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        context: Arc<dyn ContextStore>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            registry,
            policy,
            context,
            audit,
            transactional_store: None,
        }
    }

    pub fn with_transactional_store(mut self, store: Arc<dyn ToolResultStore>) -> Self {
        self.transactional_store = Some(store);
        self
    }

    pub async fn execute(
        &self,
        plan: &Plan,
        approval: Option<&Approval>,
        job: ToolExecutionJob,
        executor: &dyn ToolExecutor,
    ) -> Result<ToolExecutionOutcome, ToolRuntimeError> {
        if plan.steps.len() != 1 {
            return Err(ToolRuntimeError::Tool(ToolError::Execution {
                tool: "plan".into(),
                reason: "minimalny runtime obsługuje jeden krok".into(),
            }));
        }
        let step = &plan.steps[0];
        let manifest = self
            .registry
            .get(&step.tool)
            .ok_or_else(|| ToolError::NotRegistered(step.tool.to_string()))?;
        let decision = self
            .policy
            .evaluate_plan(&self.registry.descriptors(), plan);
        if decision.outcome == Outcome::Denied {
            let reason = decision.reasons.join("; ");
            self.record_audit(
                plan,
                manifest,
                job.started_at_unix_ms,
                job.started_at_unix_ms,
                AuditOutcome::Denied,
                &reason,
            )?;
            return Err(ToolRuntimeError::PolicyDenied(reason));
        }
        self.policy
            .require_approval(plan, approval, job.started_at_unix_ms)?;
        self.registry.validate_input(&step.tool, &step.args)?;
        if executor.tool_id() != &step.tool {
            return Err(ToolRuntimeError::Tool(ToolError::Execution {
                tool: step.tool.to_string(),
                reason: "executor nie odpowiada krokowi planu".into(),
            }));
        }

        let execution = executor
            .execute(step.args.clone(), job.started_at_unix_ms)
            .await;
        let execution = match execution {
            Ok(value) => value,
            Err(error) => {
                self.record_audit(
                    plan,
                    manifest,
                    job.started_at_unix_ms,
                    job.started_at_unix_ms,
                    AuditOutcome::Failed,
                    &error.to_string(),
                )?;
                return Err(error.into());
            }
        };
        self.registry
            .validate_output(&step.tool, &execution.output)?;
        let result = ToolResult {
            tool: step.tool.clone(),
            tool_version: manifest.tool_version.clone(),
            output: execution.output,
            summary: execution.summary,
            started_at_unix_ms: job.started_at_unix_ms,
            finished_at_unix_ms: execution.finished_at_unix_ms,
        };
        let context_item =
            tool_result_context_item(job.result_item_id, job.scope, &result, manifest.result_tier)?;
        let audit_event = ToolAuditEvent {
            task_id: plan.task_id.clone(),
            tool: manifest.descriptor.id.clone(),
            tool_version: manifest.tool_version.clone(),
            plan_hash: plan.hash().to_string(),
            started_at_unix_ms: result.started_at_unix_ms,
            finished_at_unix_ms: result.finished_at_unix_ms,
            outcome: AuditOutcome::Succeeded,
            summary: result.summary.clone(),
        };
        if let Some(store) = &self.transactional_store {
            store.commit_tool_result(context_item.clone(), audit_event)?;
        } else {
            self.context.put(context_item.clone())?;
            self.audit.record(audit_event)?;
        }
        Ok(ToolExecutionOutcome {
            result,
            context_item,
        })
    }

    fn record_audit(
        &self,
        plan: &Plan,
        manifest: &victor_tools::ToolManifest,
        started_at_unix_ms: UnixMs,
        finished_at_unix_ms: UnixMs,
        outcome: AuditOutcome,
        summary: &str,
    ) -> Result<(), ToolRuntimeError> {
        self.audit.record(ToolAuditEvent {
            task_id: plan.task_id.clone(),
            tool: manifest.descriptor.id.clone(),
            tool_version: manifest.tool_version.clone(),
            plan_hash: plan.hash().to_string(),
            started_at_unix_ms,
            finished_at_unix_ms,
            outcome,
            summary: summary.to_string(),
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionJob {
    pub result_item_id: ContextItemId,
    pub scope: ContextScope,
    pub started_at_unix_ms: UnixMs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolExecutionOutcome {
    pub result: ToolResult,
    pub context_item: ContextItem,
}

fn tool_result_context_item(
    id: ContextItemId,
    scope: ContextScope,
    result: &ToolResult,
    result_tier: ResultTier,
) -> Result<ContextItem, victor_context::ContextError> {
    let output = to_string(&result.output).unwrap_or_else(|_| "{}".into());
    let text = format!("{}: {}", result.summary, output);
    ContextItem::new(
        id,
        scope,
        ContextItemKind::ToolResult,
        match result_tier {
            ResultTier::Working => MemoryTier::Working,
            ResultTier::Evidence => MemoryTier::Evidence,
        },
        ContextContent::inline(text),
        ContextSource::new(victor_context::Origin::Tool, result.tool.as_str()),
        result.finished_at_unix_ms,
    )?
    .with_sensitivity(Sensitivity::Internal)
    .with_retention(Retention::TtlFromCreation(24 * 60 * 60 * 1_000))
}
