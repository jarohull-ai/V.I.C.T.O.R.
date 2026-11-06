//! Lokalny, UI-agnostyczny API przeglądu i lifecycle skills.
//!
//! API nie wykonuje skillów. Udostępnia manifesty i operacje decyzyjne, a
//! każdą zmianę ponownie waliduje przez `victor-skills` i `victor-policy`.

use std::sync::Arc;

use victor_policy::PolicyEngine;
use victor_skill_forge::{ForgeError, ForgeRequest, SkillForge, SkillReview};
use victor_skills::{
    SkillDecisionAudit, SkillDecisionAuditStore, SkillError, SkillId, SkillRegistry, SkillSummary,
};
use victor_tools::ToolRegistry;

#[derive(Debug, thiserror::Error)]
pub enum SkillApiError {
    #[error(transparent)]
    Forge(#[from] ForgeError),
    #[error(transparent)]
    Skill(#[from] SkillError),
    #[error("audit: {0}")]
    Audit(String),
}

pub struct SkillApi {
    forge: SkillForge,
    registry: SkillRegistry,
    tools: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    audit: Arc<dyn SkillDecisionAuditStore>,
}

impl SkillApi {
    pub fn new(
        tools: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        audit: Arc<dyn SkillDecisionAuditStore>,
    ) -> Result<Self, SkillApiError> {
        let registry = match audit.load_skill_registry().map_err(SkillApiError::Audit)? {
            Some(snapshot) => SkillRegistry::from_snapshot_json(&snapshot)?,
            None => SkillRegistry::new(),
        };
        Ok(Self {
            forge: SkillForge::new(),
            registry,
            tools,
            policy,
            audit,
        })
    }

    pub fn create_draft(&mut self, request: ForgeRequest) -> Result<SkillSummary, SkillApiError> {
        let manifest = self
            .forge
            .create_draft(request, &self.tools, &mut self.registry)?;
        self.persist_registry()?;
        self.summary(&manifest.skill_id)
    }

    pub fn list(&self) -> Vec<SkillSummary> {
        self.registry.summaries()
    }

    pub fn snapshot_json(&self) -> Result<String, SkillApiError> {
        self.registry.snapshot_json().map_err(SkillApiError::Skill)
    }

    pub fn restore_snapshot_json(&mut self, snapshot: &str) -> Result<(), SkillApiError> {
        self.registry = SkillRegistry::from_snapshot_json(snapshot)?;
        Ok(())
    }

    pub fn update_manifest(
        &mut self,
        manifest: victor_skills::SkillManifest,
    ) -> Result<SkillSummary, SkillApiError> {
        self.registry.update(manifest.clone())?;
        self.persist_registry()?;
        self.summary(&manifest.skill_id)
    }

    pub fn review(&self, id: &SkillId) -> Result<SkillReview, SkillApiError> {
        Ok(self
            .forge
            .review(id, None, &self.tools, &self.policy, &self.registry)?)
    }

    pub fn validate_and_approve_activate(
        &mut self,
        id: &SkillId,
        actor: &str,
        at_unix_ms: u64,
        reason: &str,
    ) -> Result<SkillSummary, SkillApiError> {
        self.forge
            .validate(id, &self.tools, &self.policy, &mut self.registry)?;
        self.forge.approve_and_activate_with_audit(
            id,
            &mut self.registry,
            self.audit.as_ref(),
            actor,
            at_unix_ms,
            reason,
        )?;
        self.persist_registry()?;
        self.summary(id)
    }

    pub fn disable(
        &mut self,
        id: &SkillId,
        actor: &str,
        at_unix_ms: u64,
        reason: &str,
    ) -> Result<SkillSummary, SkillApiError> {
        self.forge.disable_with_audit(
            id,
            &mut self.registry,
            self.audit.as_ref(),
            actor,
            at_unix_ms,
            reason,
        )?;
        self.persist_registry()?;
        self.summary(id)
    }

    pub fn rollback(
        &mut self,
        id: &SkillId,
        actor: &str,
        at_unix_ms: u64,
        reason: &str,
    ) -> Result<SkillSummary, SkillApiError> {
        self.forge.rollback_with_audit(
            id,
            &mut self.registry,
            self.audit.as_ref(),
            actor,
            at_unix_ms,
            reason,
        )?;
        self.persist_registry()?;
        self.summary(id)
    }

    pub fn audit_history(
        &self,
        id: Option<&SkillId>,
    ) -> Result<Vec<SkillDecisionAudit>, SkillApiError> {
        self.audit
            .list_skill_decisions(id)
            .map_err(SkillApiError::Audit)
    }

    pub fn active_manifest(
        &self,
        id: &SkillId,
    ) -> Result<&victor_skills::SkillManifest, SkillApiError> {
        Ok(self.registry.active_manifest(id)?)
    }

    fn summary(&self, id: &SkillId) -> Result<SkillSummary, SkillApiError> {
        self.registry
            .summaries()
            .into_iter()
            .find(|summary| &summary.skill_id == id)
            .ok_or_else(|| SkillApiError::Skill(SkillError::NotFound(id.to_string())))
    }

    fn persist_registry(&self) -> Result<(), SkillApiError> {
        let snapshot = self.snapshot_json()?;
        self.audit
            .save_skill_registry(&snapshot)
            .map_err(SkillApiError::Audit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use victor_domain::{Capability, RiskLevel, ToolDescriptor, ToolId};
    use victor_policy::PolicyConfig;
    use victor_skill_forge::ForgeRequest;
    use victor_skills::{SkillLifecycle, SkillStep};
    use victor_storage::SqliteContextStore;
    use victor_tools::ToolManifest;

    fn request() -> ForgeRequest {
        ForgeRequest {
            description: "odczytaj demo".into(),
            skill_id: SkillId::parse("api.demo").unwrap(),
            version: "1.0.0".into(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            workflow: vec![SkillStep {
                id: "read".into(),
                tool: ToolId::parse("demo.read").unwrap(),
                args: json!({}),
                depends_on: vec![],
            }],
            dependencies: vec![],
            author: "user".into(),
        }
    }

    #[test]
    fn local_api_exposes_review_lifecycle_and_decision_history() {
        let mut tools = ToolRegistry::new();
        tools
            .register(ToolManifest::new(
                "1.0.0",
                ToolDescriptor {
                    id: ToolId::parse("demo.read").unwrap(),
                    schema_version: 1,
                    description: "read".into(),
                    input_schema: json!({"type": "object"}),
                    output_schema: json!({"type": "object"}),
                    capabilities: vec![Capability::Network {
                        provider: "test".into(),
                    }],
                    risk: RiskLevel::R0,
                    default_timeout_ms: 1_000,
                    undo_support: false,
                    path_arguments: vec![],
                },
            ))
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let policy = Arc::new(
            PolicyEngine::new(
                PolicyConfig::new([root.path().to_path_buf()])
                    .with_enabled_tools([ToolId::parse("demo.read").unwrap()]),
            )
            .unwrap(),
        );
        let audit = Arc::new(SqliteContextStore::in_memory().unwrap());
        let mut api = SkillApi::new(Arc::new(tools), policy, audit.clone()).unwrap();
        let summary = api.create_draft(request()).unwrap();
        assert_eq!(summary.lifecycle, SkillLifecycle::Draft);
        assert_eq!(
            api.review(&summary.skill_id).unwrap().lifecycle,
            SkillLifecycle::Draft
        );
        let active = api
            .validate_and_approve_activate(&summary.skill_id, "user", 1, "approved")
            .unwrap();
        assert_eq!(active.lifecycle, SkillLifecycle::Active);
        api.disable(&summary.skill_id, "user", 2, "pause").unwrap();
        assert_eq!(api.audit_history(None).unwrap().len(), 3);
        assert!(api.active_manifest(&summary.skill_id).is_err());
    }
}
