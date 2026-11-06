//! Bezpieczne tworzenie deklaratywnych skills.
//!
//! Forge nie generuje kodu i nie rejestruje narzędzi. Buduje wyłącznie graf z
//! istniejących manifestów `victor-tools`, a decyzję o policy i lifecycle
//! pozostawia `victor-skills`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use victor_domain::RiskLevel;
use victor_policy::PolicyEngine;
use victor_skills::{
    SkillDecisionAudit, SkillDecisionAuditSink, SkillDependency, SkillError, SkillId,
    SkillLifecycle, SkillManifest, SkillProvenance, SkillRegistry, SkillStep,
};
use victor_tools::{ToolError, ToolRegistry};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForgeRequest {
    pub description: String,
    pub skill_id: SkillId,
    pub version: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub workflow: Vec<SkillStep>,
    pub dependencies: Vec<SkillDependency>,
    pub author: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunStep {
    pub id: String,
    pub tool: String,
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunReport {
    pub skill_id: SkillId,
    pub manifest_hash: String,
    pub steps: Vec<DryRunStep>,
    pub policy_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDiff {
    pub changed_fields: Vec<String>,
    pub added_steps: Vec<String>,
    pub removed_steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillReview {
    pub manifest: SkillManifest,
    pub lifecycle: SkillLifecycle,
    pub dry_run: DryRunReport,
    pub diff: ManifestDiff,
}

struct DecisionContext<'a> {
    sink: &'a dyn SkillDecisionAuditSink,
    actor: &'a str,
    at_unix_ms: u64,
    reason: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("opis skill jest pusty albo zbyt długi")]
    InvalidDescription,
    #[error("forge nie może użyć narzędzia z ryzykiem {0:?}: {1}")]
    UnsupportedRisk(RiskLevel, String),
    #[error("nieznane narzędzie w recipe: {0}")]
    UnknownTool(String),
    #[error(transparent)]
    Skill(#[from] SkillError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("nie udało się zapisać decyzji lifecycle: {0}")]
    Audit(String),
}

#[derive(Debug, Default)]
pub struct SkillForge;

impl SkillForge {
    pub fn new() -> Self {
        Self
    }

    pub fn create_draft(
        &self,
        request: ForgeRequest,
        tools: &ToolRegistry,
        skills: &mut SkillRegistry,
    ) -> Result<SkillManifest, ForgeError> {
        if request.description.trim().is_empty() || request.description.len() > 4_096 {
            return Err(ForgeError::InvalidDescription);
        }
        for step in &request.workflow {
            let tool = tools
                .get(&step.tool)
                .ok_or_else(|| ForgeError::UnknownTool(step.tool.to_string()))?;
            if tool.descriptor.risk > RiskLevel::R0 {
                return Err(ForgeError::UnsupportedRisk(
                    tool.descriptor.risk,
                    step.tool.to_string(),
                ));
            }
            tools.validate_input(&step.tool, &step.args)?;
        }
        let manifest = SkillManifest::new(
            request.skill_id,
            request.version,
            request.input_schema,
            request.output_schema,
            request.workflow,
            request.dependencies,
            SkillProvenance {
                author: request.author,
                generator: Some("victor-skill-forge".into()),
                source: request.description,
            },
        )?;
        skills.install(manifest.clone())?;
        Ok(manifest)
    }

    pub fn dry_run(
        &self,
        id: &SkillId,
        tools: &ToolRegistry,
        policy: &PolicyEngine,
        skills: &SkillRegistry,
    ) -> Result<DryRunReport, ForgeError> {
        let snapshot = skills.snapshot_json().map_err(ForgeError::Skill)?;
        let mut checked = SkillRegistry::from_snapshot_json(&snapshot)?;
        checked.validate_with_policy(id, tools, policy)?;
        let manifest = checked.manifest(id)?;
        let steps = manifest
            .workflow
            .iter()
            .map(|step| {
                let risk = tools
                    .get(&step.tool)
                    .map(|tool| tool.descriptor.risk)
                    .unwrap_or(RiskLevel::R3);
                DryRunStep {
                    id: step.id.clone(),
                    tool: step.tool.to_string(),
                    risk,
                }
            })
            .collect();
        Ok(DryRunReport {
            skill_id: id.clone(),
            manifest_hash: manifest.hash.clone(),
            steps,
            policy_allowed: true,
        })
    }

    pub fn validate(
        &self,
        id: &SkillId,
        tools: &ToolRegistry,
        policy: &PolicyEngine,
        skills: &mut SkillRegistry,
    ) -> Result<(), ForgeError> {
        skills.validate_with_policy(id, tools, policy)?;
        Ok(())
    }

    pub fn review(
        &self,
        id: &SkillId,
        previous: Option<&SkillManifest>,
        tools: &ToolRegistry,
        policy: &PolicyEngine,
        skills: &SkillRegistry,
    ) -> Result<SkillReview, ForgeError> {
        let manifest = skills.manifest(id)?.clone();
        let dry_run = self.dry_run(id, tools, policy, skills)?;
        Ok(SkillReview {
            lifecycle: skills.state(id)?,
            diff: manifest_diff(previous, &manifest),
            manifest,
            dry_run,
        })
    }

    pub fn approve_and_activate(
        &self,
        id: &SkillId,
        skills: &mut SkillRegistry,
    ) -> Result<(), ForgeError> {
        if skills.state(id)? != SkillLifecycle::Validated {
            return Err(SkillError::InvalidTransition {
                from: skills.state(id)?,
                to: SkillLifecycle::Approved,
            }
            .into());
        }
        skills.approve(id)?;
        skills.activate(id)?;
        Ok(())
    }

    pub fn approve_and_activate_with_audit(
        &self,
        id: &SkillId,
        skills: &mut SkillRegistry,
        sink: &dyn SkillDecisionAuditSink,
        actor: &str,
        at_unix_ms: u64,
        reason: &str,
    ) -> Result<(), ForgeError> {
        let context = DecisionContext {
            sink,
            actor,
            at_unix_ms,
            reason,
        };
        self.audited_transition(id, SkillLifecycle::Approved, skills, &context)?;
        self.audited_transition(id, SkillLifecycle::Active, skills, &context)
    }

    pub fn disable_with_audit(
        &self,
        id: &SkillId,
        skills: &mut SkillRegistry,
        sink: &dyn SkillDecisionAuditSink,
        actor: &str,
        at_unix_ms: u64,
        reason: &str,
    ) -> Result<(), ForgeError> {
        let context = DecisionContext {
            sink,
            actor,
            at_unix_ms,
            reason,
        };
        self.audited_transition(id, SkillLifecycle::Disabled, skills, &context)
    }

    pub fn rollback_with_audit(
        &self,
        id: &SkillId,
        skills: &mut SkillRegistry,
        sink: &dyn SkillDecisionAuditSink,
        actor: &str,
        at_unix_ms: u64,
        reason: &str,
    ) -> Result<(), ForgeError> {
        let from = skills.state(id)?;
        skills.rollback(id)?;
        let context = DecisionContext {
            sink,
            actor,
            at_unix_ms,
            reason,
        };
        self.record_decision(id, from, SkillLifecycle::Draft, skills, &context)
    }

    fn audited_transition(
        &self,
        id: &SkillId,
        target: SkillLifecycle,
        skills: &mut SkillRegistry,
        context: &DecisionContext<'_>,
    ) -> Result<(), ForgeError> {
        let from = skills.state(id)?;
        match target {
            SkillLifecycle::Approved => skills.approve(id)?,
            SkillLifecycle::Active => skills.activate(id)?,
            SkillLifecycle::Disabled => skills.disable(id)?,
            _ => {
                return Err(SkillError::InvalidTransition { from, to: target }.into());
            }
        }
        self.record_decision(id, from, target, skills, context)
    }

    fn record_decision(
        &self,
        id: &SkillId,
        from: SkillLifecycle,
        to: SkillLifecycle,
        skills: &SkillRegistry,
        context: &DecisionContext<'_>,
    ) -> Result<(), ForgeError> {
        let manifest = skills.manifest(id)?;
        context
            .sink
            .record_skill_decision(SkillDecisionAudit {
                skill_id: id.clone(),
                version: manifest.version.clone(),
                manifest_hash: manifest.hash.clone(),
                from,
                to,
                actor: context.actor.into(),
                reason: context.reason.into(),
                at_unix_ms: context.at_unix_ms,
            })
            .map_err(ForgeError::Audit)
    }
}

fn manifest_diff(previous: Option<&SkillManifest>, current: &SkillManifest) -> ManifestDiff {
    let Some(previous) = previous else {
        return ManifestDiff {
            changed_fields: vec!["manifest".into()],
            added_steps: current
                .workflow
                .iter()
                .map(|step| step.id.clone())
                .collect(),
            removed_steps: vec![],
        };
    };
    let mut changed_fields = Vec::new();
    if previous.version != current.version {
        changed_fields.push("version".into());
    }
    if previous.input_schema != current.input_schema {
        changed_fields.push("input_schema".into());
    }
    if previous.output_schema != current.output_schema {
        changed_fields.push("output_schema".into());
    }
    if previous.dependencies != current.dependencies {
        changed_fields.push("dependencies".into());
    }
    if previous.provenance != current.provenance {
        changed_fields.push("provenance".into());
    }
    let previous_steps: std::collections::BTreeSet<_> = previous
        .workflow
        .iter()
        .map(|step| step.id.as_str())
        .collect();
    let current_steps: std::collections::BTreeSet<_> = current
        .workflow
        .iter()
        .map(|step| step.id.as_str())
        .collect();
    ManifestDiff {
        changed_fields,
        added_steps: current_steps
            .difference(&previous_steps)
            .map(|id| (*id).into())
            .collect(),
        removed_steps: previous_steps
            .difference(&current_steps)
            .map(|id| (*id).into())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use victor_domain::{Capability, ToolDescriptor, ToolId};
    use victor_policy::PolicyConfig;
    use victor_storage::SqliteContextStore;
    use victor_tools::ToolManifest;

    fn tools() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry
            .register(ToolManifest::new(
                "1.0.0",
                ToolDescriptor {
                    id: ToolId::parse("demo.read").unwrap(),
                    schema_version: 1,
                    description: "read-only demo".into(),
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
        registry
            .register(ToolManifest::new(
                "1.0.0",
                ToolDescriptor {
                    id: ToolId::parse("demo.write").unwrap(),
                    schema_version: 1,
                    description: "mutating demo".into(),
                    input_schema: json!({"type": "object"}),
                    output_schema: json!({"type": "object"}),
                    capabilities: vec![Capability::Network {
                        provider: "test".into(),
                    }],
                    risk: RiskLevel::R1,
                    default_timeout_ms: 1_000,
                    undo_support: true,
                    path_arguments: vec![],
                },
            ))
            .unwrap();
        registry
    }

    fn request() -> ForgeRequest {
        ForgeRequest {
            description: "Przeczytaj dane demonstracyjne".into(),
            skill_id: SkillId::parse("demo.forged").unwrap(),
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
    fn forge_runs_draft_dry_run_validation_and_activation() {
        let forge = SkillForge::new();
        let tools = tools();
        let root = tempfile::tempdir().unwrap();
        let policy = PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("demo.read").unwrap()]),
        )
        .unwrap();
        let mut skills = SkillRegistry::new();
        let manifest = forge.create_draft(request(), &tools, &mut skills).unwrap();
        assert_eq!(
            skills.state(&manifest.skill_id).unwrap(),
            SkillLifecycle::Draft
        );
        let report = forge
            .dry_run(&manifest.skill_id, &tools, &policy, &skills)
            .unwrap();
        assert_eq!(report.steps[0].tool, "demo.read");
        let review = forge
            .review(&manifest.skill_id, None, &tools, &policy, &skills)
            .unwrap();
        assert_eq!(review.lifecycle, SkillLifecycle::Draft);
        assert_eq!(review.diff.changed_fields, vec!["manifest"]);
        assert_eq!(
            skills.state(&manifest.skill_id).unwrap(),
            SkillLifecycle::Draft
        );
        forge
            .validate(&manifest.skill_id, &tools, &policy, &mut skills)
            .unwrap();
        let audit = SqliteContextStore::in_memory().unwrap();
        forge
            .approve_and_activate_with_audit(
                &manifest.skill_id,
                &mut skills,
                &audit,
                "user",
                100,
                "review accepted",
            )
            .unwrap();
        assert_eq!(
            skills.state(&manifest.skill_id).unwrap(),
            SkillLifecycle::Active
        );
        forge
            .disable_with_audit(
                &manifest.skill_id,
                &mut skills,
                &audit,
                "user",
                101,
                "temporarily disabled",
            )
            .unwrap();
        assert_eq!(audit.skill_decision_count().unwrap(), 3);
        let mut changed = manifest.clone();
        changed.version = "1.0.1".into();
        changed.hash = changed.computed_hash().unwrap();
        skills.update(changed).unwrap();
        forge
            .rollback_with_audit(
                &manifest.skill_id,
                &mut skills,
                &audit,
                "user",
                102,
                "rollback after review",
            )
            .unwrap();
        assert_eq!(
            skills.manifest(&manifest.skill_id).unwrap().hash,
            manifest.hash
        );
        forge
            .validate(&manifest.skill_id, &tools, &policy, &mut skills)
            .unwrap();
        forge
            .approve_and_activate_with_audit(
                &manifest.skill_id,
                &mut skills,
                &audit,
                "user",
                103,
                "re-activated after rollback",
            )
            .unwrap();
        assert_eq!(
            skills.state(&manifest.skill_id).unwrap(),
            SkillLifecycle::Active
        );
        assert_eq!(audit.skill_decision_count().unwrap(), 6);
        assert!(serde_json::to_string(&manifest)
            .unwrap()
            .contains("victor-skill-forge"));
        assert!(!serde_json::to_string(&manifest)
            .unwrap()
            .contains("capabilities"));
    }

    #[test]
    fn forge_rejects_unknown_tools_and_non_read_only_tools() {
        let forge = SkillForge::new();
        let mut bad = request();
        bad.workflow[0].tool = ToolId::parse("missing.tool").unwrap();
        assert!(matches!(
            forge.create_draft(bad, &tools(), &mut SkillRegistry::new()),
            Err(ForgeError::UnknownTool(_))
        ));
        let mut mutating = request();
        mutating.workflow[0].tool = ToolId::parse("demo.write").unwrap();
        assert!(matches!(
            forge.create_draft(mutating, &tools(), &mut SkillRegistry::new()),
            Err(ForgeError::UnsupportedRisk(RiskLevel::R1, _))
        ));
    }
}
