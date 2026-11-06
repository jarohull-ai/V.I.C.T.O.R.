//! Deklaratywne skills V.I.C.T.O.R.
//!
//! Skill nie zawiera kodu wykonywalnego ani własnych capabilities. Jest
//! grafem wywołań narzędzi z registry, zatwierdzanym przez lifecycle.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use victor_domain::{Plan, PlanStep, RiskLevel, ToolId};
use victor_policy::{Outcome, PolicyEngine};
use victor_tools::{validate_json_schema, ToolError, ToolRegistry};

pub const SKILL_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(String);

impl SkillId {
    pub fn parse(raw: &str) -> Result<Self, SkillError> {
        let valid = !raw.is_empty()
            && raw.len() <= 64
            && raw.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && raw.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_')
            });
        if valid {
            Ok(Self(raw.into()))
        } else {
            Err(SkillError::InvalidManifest("nieprawidłowy skill_id".into()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProvenance {
    pub author: String,
    pub generator: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDependency {
    pub skill: SkillId,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillStep {
    pub id: String,
    pub tool: ToolId,
    pub args: Value,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillManifest {
    pub manifest_version: u32,
    pub skill_id: SkillId,
    pub version: String,
    pub hash: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub workflow: Vec<SkillStep>,
    pub dependencies: Vec<SkillDependency>,
    pub provenance: SkillProvenance,
}

impl SkillManifest {
    pub fn new(
        skill_id: SkillId,
        version: impl Into<String>,
        input_schema: Value,
        output_schema: Value,
        workflow: Vec<SkillStep>,
        dependencies: Vec<SkillDependency>,
        provenance: SkillProvenance,
    ) -> Result<Self, SkillError> {
        let mut manifest = Self {
            manifest_version: SKILL_MANIFEST_VERSION,
            skill_id,
            version: version.into(),
            hash: String::new(),
            input_schema,
            output_schema,
            workflow,
            dependencies,
            provenance,
        };
        manifest.hash = manifest.computed_hash()?;
        Ok(manifest)
    }

    pub fn computed_hash(&self) -> Result<String, SkillError> {
        let mut unsigned = self.clone();
        unsigned.hash.clear();
        let bytes = serde_json::to_vec(&unsigned)
            .map_err(|error| SkillError::InvalidManifest(error.to_string()))?;
        let digest = Sha256::digest(bytes);
        Ok(format!("sha256:{}", hex_string(&digest)))
    }

    pub fn validate_self(&self) -> Result<(), SkillError> {
        if self.manifest_version != SKILL_MANIFEST_VERSION {
            return Err(SkillError::InvalidManifest(
                "nieobsługiwana wersja manifestu".into(),
            ));
        }
        if self.version.trim().is_empty() || self.version.len() > 32 {
            return Err(SkillError::InvalidManifest(
                "version nie może być pusta ani zbyt długa".into(),
            ));
        }
        if self.provenance.author.trim().is_empty() || self.provenance.source.trim().is_empty() {
            return Err(SkillError::InvalidManifest(
                "provenance wymaga author i source".into(),
            ));
        }
        validate_json_schema(&self.input_schema, "skill.input_schema")
            .map_err(SkillError::InvalidManifest)?;
        validate_json_schema(&self.output_schema, "skill.output_schema")
            .map_err(SkillError::InvalidManifest)?;
        let expected = self.computed_hash()?;
        if self.hash != expected {
            return Err(SkillError::HashMismatch {
                expected,
                actual: self.hash.clone(),
            });
        }
        validate_workflow_graph(&self.workflow)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillLifecycle {
    Draft,
    Validated,
    Approved,
    Active,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDecisionAudit {
    pub skill_id: SkillId,
    pub version: String,
    pub manifest_hash: String,
    pub from: SkillLifecycle,
    pub to: SkillLifecycle,
    pub actor: String,
    pub reason: String,
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub skill_id: SkillId,
    pub version: String,
    pub manifest_hash: String,
    pub lifecycle: SkillLifecycle,
}

pub trait SkillDecisionAuditStore: SkillDecisionAuditSink {
    fn list_skill_decisions(
        &self,
        skill_id: Option<&SkillId>,
    ) -> Result<Vec<SkillDecisionAudit>, String>;

    fn load_skill_registry(&self) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn save_skill_registry(&self, _snapshot: &str) -> Result<(), String> {
        Ok(())
    }
}

pub trait SkillDecisionAuditSink: Send + Sync {
    fn record_skill_decision(&self, event: SkillDecisionAudit) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("nieprawidłowy manifest skill: {0}")]
    InvalidManifest(String),
    #[error("hash skill nie pasuje: expected {expected}, actual {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("skill nie istnieje: {0}")]
    NotFound(String),
    #[error("nieznane narzędzie w workflow: {0}")]
    UnknownTool(String),
    #[error("cykl w workflow skill: {0}")]
    WorkflowCycle(String),
    #[error("cykl zależności skills: {0}")]
    DependencyCycle(String),
    #[error("nieprawidłowy lifecycle: {from:?} -> {to:?}")]
    InvalidTransition {
        from: SkillLifecycle,
        to: SkillLifecycle,
    },
    #[error("polityka odrzuciła skill: {0}")]
    PolicyDenied(String),
    #[error("błąd registry narzędzi: {0}")]
    Tool(#[from] ToolError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillRecord {
    manifest: SkillManifest,
    state: SkillLifecycle,
    history: Vec<SkillManifest>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SkillRegistry {
    records: BTreeMap<String, SkillRecord>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&mut self, manifest: SkillManifest) -> Result<(), SkillError> {
        manifest.validate_self()?;
        let id = manifest.skill_id.as_str().to_string();
        if self.records.contains_key(&id) {
            return Err(SkillError::InvalidManifest("skill już istnieje".into()));
        }
        self.ensure_dependencies_exist_and_acyclic(&manifest)?;
        self.records.insert(
            id,
            SkillRecord {
                manifest,
                state: SkillLifecycle::Draft,
                history: Vec::new(),
            },
        );
        Ok(())
    }

    pub fn update(&mut self, manifest: SkillManifest) -> Result<(), SkillError> {
        manifest.validate_self()?;
        let id = manifest.skill_id.as_str().to_string();
        self.ensure_dependencies_exist_and_acyclic(&manifest)?;
        let record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| SkillError::NotFound(id.clone()))?;
        if !matches!(
            record.state,
            SkillLifecycle::Draft | SkillLifecycle::Disabled
        ) {
            return Err(SkillError::InvalidTransition {
                from: record.state,
                to: SkillLifecycle::Draft,
            });
        }
        record.history.push(record.manifest.clone());
        record.manifest = manifest;
        record.state = SkillLifecycle::Draft;
        Ok(())
    }

    pub fn validate(&mut self, id: &SkillId, tools: &ToolRegistry) -> Result<(), SkillError> {
        let record = self.record(id)?;
        validate_workflow_tools(&record.manifest, tools)?;
        if record.state != SkillLifecycle::Draft {
            return Err(SkillError::InvalidTransition {
                from: record.state,
                to: SkillLifecycle::Validated,
            });
        }
        self.records.get_mut(id.as_str()).unwrap().state = SkillLifecycle::Validated;
        Ok(())
    }

    pub fn validate_with_policy(
        &mut self,
        id: &SkillId,
        tools: &ToolRegistry,
        policy: &PolicyEngine,
    ) -> Result<(), SkillError> {
        self.validate(id, tools)?;
        let manifest = &self.record(id)?.manifest;
        let steps = manifest
            .workflow
            .iter()
            .map(|step| {
                let risk = tools
                    .get(&step.tool)
                    .map(|tool| tool.descriptor.risk)
                    .unwrap_or(RiskLevel::R3);
                PlanStep::new(step.tool.clone(), step.id.clone(), step.args.clone(), risk)
            })
            .collect();
        let plan = Plan::new(
            format!("skill:{}", manifest.skill_id),
            1,
            format!("validate:{}", manifest.skill_id),
            steps,
        )
        .map_err(|e| SkillError::PolicyDenied(e.to_string()))?;
        let decision = policy.evaluate_plan(&tools.descriptors(), &plan);
        if decision.outcome == Outcome::Denied {
            self.records.get_mut(id.as_str()).unwrap().state = SkillLifecycle::Draft;
            return Err(SkillError::PolicyDenied(decision.reasons.join("; ")));
        }
        Ok(())
    }

    pub fn approve(&mut self, id: &SkillId) -> Result<(), SkillError> {
        self.transition(id, SkillLifecycle::Approved)
    }

    pub fn activate(&mut self, id: &SkillId) -> Result<(), SkillError> {
        self.transition(id, SkillLifecycle::Active)
    }

    pub fn disable(&mut self, id: &SkillId) -> Result<(), SkillError> {
        self.transition(id, SkillLifecycle::Disabled)
    }

    pub fn rollback(&mut self, id: &SkillId) -> Result<(), SkillError> {
        let record = self.record_mut(id)?;
        let previous = record
            .history
            .pop()
            .ok_or_else(|| SkillError::InvalidManifest("brak wersji do rollbacku".into()))?;
        record.manifest = previous;
        record.state = SkillLifecycle::Draft;
        Ok(())
    }

    pub fn state(&self, id: &SkillId) -> Result<SkillLifecycle, SkillError> {
        Ok(self.record(id)?.state)
    }

    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.records
            .values()
            .map(|record| SkillSummary {
                skill_id: record.manifest.skill_id.clone(),
                version: record.manifest.version.clone(),
                manifest_hash: record.manifest.hash.clone(),
                lifecycle: record.state,
            })
            .collect()
    }

    pub fn manifest(&self, id: &SkillId) -> Result<&SkillManifest, SkillError> {
        Ok(&self.record(id)?.manifest)
    }

    pub fn active_manifest(&self, id: &SkillId) -> Result<&SkillManifest, SkillError> {
        let record = self.record(id)?;
        if record.state != SkillLifecycle::Active {
            return Err(SkillError::InvalidTransition {
                from: record.state,
                to: SkillLifecycle::Active,
            });
        }
        Ok(&record.manifest)
    }

    pub fn snapshot_json(&self) -> Result<String, SkillError> {
        serde_json::to_string(self).map_err(|error| SkillError::InvalidManifest(error.to_string()))
    }

    pub fn from_snapshot_json(snapshot: &str) -> Result<Self, SkillError> {
        serde_json::from_str(snapshot)
            .map_err(|error| SkillError::InvalidManifest(error.to_string()))
    }

    fn transition(&mut self, id: &SkillId, target: SkillLifecycle) -> Result<(), SkillError> {
        let record = self.record_mut(id)?;
        let allowed = matches!(
            (record.state, target),
            (SkillLifecycle::Draft, SkillLifecycle::Validated)
                | (SkillLifecycle::Validated, SkillLifecycle::Approved)
                | (SkillLifecycle::Approved, SkillLifecycle::Active)
                | (SkillLifecycle::Active, SkillLifecycle::Disabled)
        );
        if !allowed {
            return Err(SkillError::InvalidTransition {
                from: record.state,
                to: target,
            });
        }
        record.state = target;
        Ok(())
    }

    fn record(&self, id: &SkillId) -> Result<&SkillRecord, SkillError> {
        self.records
            .get(id.as_str())
            .ok_or_else(|| SkillError::NotFound(id.to_string()))
    }

    fn record_mut(&mut self, id: &SkillId) -> Result<&mut SkillRecord, SkillError> {
        self.records
            .get_mut(id.as_str())
            .ok_or_else(|| SkillError::NotFound(id.to_string()))
    }

    fn ensure_dependencies_exist_and_acyclic(
        &self,
        manifest: &SkillManifest,
    ) -> Result<(), SkillError> {
        for dependency in &manifest.dependencies {
            if !self.records.contains_key(dependency.skill.as_str())
                && dependency.skill != manifest.skill_id
            {
                return Err(SkillError::NotFound(dependency.skill.to_string()));
            }
        }
        let mut graph = BTreeMap::<String, Vec<String>>::new();
        for (id, record) in &self.records {
            graph.insert(
                id.clone(),
                record
                    .manifest
                    .dependencies
                    .iter()
                    .map(|d| d.skill.to_string())
                    .collect(),
            );
        }
        graph.insert(
            manifest.skill_id.to_string(),
            manifest
                .dependencies
                .iter()
                .map(|d| d.skill.to_string())
                .collect(),
        );
        if has_cycle(&graph) {
            return Err(SkillError::DependencyCycle(manifest.skill_id.to_string()));
        }
        Ok(())
    }
}

fn validate_workflow_tools(
    manifest: &SkillManifest,
    tools: &ToolRegistry,
) -> Result<(), SkillError> {
    for step in &manifest.workflow {
        let tool = tools
            .get(&step.tool)
            .ok_or_else(|| SkillError::UnknownTool(step.tool.to_string()))?;
        tools.validate_input(&step.tool, &step.args)?;
        if tool.descriptor.risk > RiskLevel::R0 {
            return Err(SkillError::PolicyDenied(format!(
                "skill read-only zawiera narzędzie {} z ryzykiem {:?}",
                step.tool, tool.descriptor.risk
            )));
        }
    }
    Ok(())
}

fn validate_workflow_graph(workflow: &[SkillStep]) -> Result<(), SkillError> {
    let ids: BTreeSet<&str> = workflow.iter().map(|step| step.id.as_str()).collect();
    if ids.len() != workflow.len() {
        return Err(SkillError::InvalidManifest("duplikat id kroku".into()));
    }
    let mut graph = BTreeMap::new();
    for step in workflow {
        if step.id.trim().is_empty() {
            return Err(SkillError::InvalidManifest("puste id kroku".into()));
        }
        for dependency in &step.depends_on {
            if !ids.contains(dependency.as_str()) {
                return Err(SkillError::InvalidManifest(format!(
                    "krok {} zależy od nieistniejącego kroku {}",
                    step.id, dependency
                )));
            }
        }
        graph.insert(step.id.clone(), step.depends_on.clone());
    }
    if has_cycle(&graph) {
        return Err(SkillError::WorkflowCycle("workflow".into()));
    }
    Ok(())
}

fn has_cycle(graph: &BTreeMap<String, Vec<String>>) -> bool {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> bool {
        if visiting.contains(node) {
            return true;
        }
        if visited.contains(node) {
            return false;
        }
        visiting.insert(node.into());
        if graph
            .get(node)
            .into_iter()
            .flatten()
            .any(|next| visit(next, graph, visiting, visited))
        {
            return true;
        }
        visiting.remove(node);
        visited.insert(node.into());
        false
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    graph
        .keys()
        .any(|node| visit(node, graph, &mut visiting, &mut visited))
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn research_summary_skill() -> Result<SkillManifest, SkillError> {
    SkillManifest::new(
        SkillId::parse("research.summary")?,
        "1.0.0",
        serde_json::json!({
            "type": "object",
            "required": ["url"],
            "properties": {"url": {"type": "string"}}
        }),
        serde_json::json!({
            "type": "object",
            "required": ["summary"],
            "properties": {"summary": {"type": "string"}}
        }),
        vec![
            SkillStep {
                id: "fetch".into(),
                tool: ToolId::parse("web.fetch").unwrap(),
                args: serde_json::json!({"url": "https://allowed.example/article"}),
                depends_on: vec![],
            },
            SkillStep {
                id: "context".into(),
                tool: ToolId::parse("context.read").unwrap(),
                args: serde_json::json!({"source_step": "fetch"}),
                depends_on: vec!["fetch".into()],
            },
            SkillStep {
                id: "summary".into(),
                tool: ToolId::parse("summary.create").unwrap(),
                args: serde_json::json!({"source_step": "context"}),
                depends_on: vec!["context".into()],
            },
        ],
        vec![],
        SkillProvenance {
            author: "victor-core".into(),
            generator: None,
            source: "built-in declarative demo".into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use victor_domain::ToolDescriptor;
    use victor_tools::ToolManifest;

    fn tool(id: &str) -> ToolManifest {
        ToolManifest::new(
            "1.0.0",
            ToolDescriptor {
                id: ToolId::parse(id).unwrap(),
                schema_version: 1,
                description: "test".into(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                capabilities: vec![],
                risk: RiskLevel::R0,
                default_timeout_ms: 1000,
                undo_support: false,
                path_arguments: vec![],
            },
        )
    }

    fn registry(ids: &[&str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for id in ids {
            registry.register(tool(id)).unwrap();
        }
        registry
    }

    fn simple_skill(step: SkillStep) -> SkillManifest {
        SkillManifest::new(
            SkillId::parse("demo.skill").unwrap(),
            "1.0.0",
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec![step],
            vec![],
            SkillProvenance {
                author: "test".into(),
                generator: Some("test-generator".into()),
                source: "fixture".into(),
            },
        )
        .unwrap()
    }

    fn named_skill(id: &str, dependencies: Vec<SkillDependency>) -> SkillManifest {
        SkillManifest::new(
            SkillId::parse(id).unwrap(),
            "1.0.0",
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec![],
            dependencies,
            SkillProvenance {
                author: "test".into(),
                generator: None,
                source: "fixture".into(),
            },
        )
        .unwrap()
    }

    #[test]
    fn lifecycle_and_rollback_restore_previous_manifest() {
        let mut registry = SkillRegistry::new();
        let skill = simple_skill(SkillStep {
            id: "step".into(),
            tool: ToolId::parse("demo.read").unwrap(),
            args: json!({}),
            depends_on: vec![],
        });
        registry.install(skill.clone()).unwrap();
        let tools = super::tests::registry(&["demo.read"]);
        registry.validate(&skill.skill_id, &tools).unwrap();
        registry.approve(&skill.skill_id).unwrap();
        registry.activate(&skill.skill_id).unwrap();
        registry.disable(&skill.skill_id).unwrap();

        let updated = simple_skill(SkillStep {
            id: "step".into(),
            tool: ToolId::parse("demo.read").unwrap(),
            args: json!({"version": 2}),
            depends_on: vec![],
        });
        registry.update(updated).unwrap();
        registry.rollback(&skill.skill_id).unwrap();
        assert_eq!(registry.manifest(&skill.skill_id).unwrap().hash, skill.hash);
        assert_eq!(
            registry.state(&skill.skill_id).unwrap(),
            SkillLifecycle::Draft
        );
    }

    #[test]
    fn unknown_tools_and_workflow_cycles_are_rejected() {
        let mut registry = SkillRegistry::new();
        let unknown = simple_skill(SkillStep {
            id: "step".into(),
            tool: ToolId::parse("missing.tool").unwrap(),
            args: json!({}),
            depends_on: vec![],
        });
        registry.install(unknown.clone()).unwrap();
        assert!(matches!(
            registry.validate(&unknown.skill_id, &ToolRegistry::new()),
            Err(SkillError::UnknownTool(_))
        ));

        let cycle = SkillManifest::new(
            SkillId::parse("cycle.skill").unwrap(),
            "1.0.0",
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec![
                SkillStep {
                    id: "a".into(),
                    tool: ToolId::parse("demo.read").unwrap(),
                    args: json!({}),
                    depends_on: vec!["b".into()],
                },
                SkillStep {
                    id: "b".into(),
                    tool: ToolId::parse("demo.read").unwrap(),
                    args: json!({}),
                    depends_on: vec!["a".into()],
                },
            ],
            vec![],
            SkillProvenance {
                author: "test".into(),
                generator: None,
                source: "fixture".into(),
            },
        );
        assert!(matches!(
            cycle.and_then(|manifest| manifest.validate_self()),
            Err(SkillError::WorkflowCycle(_))
        ));

        let mut dependencies = SkillRegistry::new();
        let a = named_skill("a.skill", vec![]);
        dependencies.install(a.clone()).unwrap();
        let b = named_skill(
            "b.skill",
            vec![SkillDependency {
                skill: a.skill_id.clone(),
                version: "1.0.0".into(),
            }],
        );
        dependencies.install(b.clone()).unwrap();
        let a_cycle = named_skill(
            "a.skill",
            vec![SkillDependency {
                skill: b.skill_id,
                version: "1.0.0".into(),
            }],
        );
        assert!(matches!(
            dependencies.update(a_cycle),
            Err(SkillError::DependencyCycle(_))
        ));
    }

    #[test]
    fn capabilities_are_derived_from_tools_and_scope_policy_can_reject() {
        let skill = simple_skill(SkillStep {
            id: "step".into(),
            tool: ToolId::parse("demo.read").unwrap(),
            args: json!({}),
            depends_on: vec![],
        });
        let tools = registry(&["demo.read"]);
        let root = tempfile::tempdir().unwrap();
        let policy = PolicyEngine::new(
            victor_policy::PolicyConfig::new([root.path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("demo.read").unwrap()]),
        )
        .unwrap();
        let mut skills = SkillRegistry::new();
        skills.install(skill.clone()).unwrap();
        skills
            .validate_with_policy(&skill.skill_id, &tools, &policy)
            .unwrap();
        assert_eq!(
            skills.state(&skill.skill_id).unwrap(),
            SkillLifecycle::Validated
        );
        assert!(serde_json::to_string(&skill)
            .unwrap()
            .contains("provenance"));
        assert!(!serde_json::to_string(&skill)
            .unwrap()
            .contains("capabilities"));

        let mut scoped_tools = ToolRegistry::new();
        scoped_tools
            .register(ToolManifest::new(
                "1.0.0",
                ToolDescriptor {
                    id: ToolId::parse("files.read").unwrap(),
                    schema_version: 1,
                    description: "read a file".into(),
                    input_schema: json!({
                        "type": "object",
                        "required": ["path"],
                        "properties": {"path": {"type": "string"}}
                    }),
                    output_schema: json!({"type": "object"}),
                    capabilities: vec![victor_domain::Capability::FilesystemRead {
                        scope: victor_domain::PathScope::new(root.path()),
                    }],
                    risk: RiskLevel::R0,
                    default_timeout_ms: 1000,
                    undo_support: false,
                    path_arguments: vec!["path".into()],
                },
            ))
            .unwrap();
        let outside = simple_skill(SkillStep {
            id: "step".into(),
            tool: ToolId::parse("files.read").unwrap(),
            args: json!({"path": "/etc/hosts"}),
            depends_on: vec![],
        });
        skills.approve(&skill.skill_id).unwrap();
        skills.activate(&skill.skill_id).unwrap();
        skills.disable(&skill.skill_id).unwrap();
        skills.update(outside).unwrap();
        let error = skills
            .validate_with_policy(&skill.skill_id, &scoped_tools, &policy)
            .unwrap_err();
        assert!(matches!(error, SkillError::PolicyDenied(_)));
        assert_eq!(
            skills.state(&skill.skill_id).unwrap(),
            SkillLifecycle::Draft
        );
    }

    #[test]
    fn research_summary_is_declarative_three_tool_workflow() {
        let skill = research_summary_skill().unwrap();
        assert_eq!(skill.workflow.len(), 3);
        assert_eq!(skill.workflow[0].tool.as_str(), "web.fetch");
        assert_eq!(skill.workflow[2].tool.as_str(), "summary.create");
        assert_eq!(skill.provenance.author, "victor-core");
    }
}
