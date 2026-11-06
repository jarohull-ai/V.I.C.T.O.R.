use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;
use victor_domain::{
    Approval, ApprovalKind, Capability, PathScope, Plan, PlanStep, RiskLevel, ToolDescriptor,
    ToolId, UnixMs,
};

use crate::error::PolicyError;
use crate::path_guard::PathGuard;

/// Wynik oceny polityki. `Denied` nie jest błędem technicznym — to świadoma
/// decyzja, którą UI pokazuje razem z powodami.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Allowed,
    ApprovalRequired,
    Denied,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Allowed => "allowed",
            Outcome::ApprovalRequired => "approval_required",
            Outcome::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub allowed_roots: Vec<PathBuf>,
    pub enabled_tools: Vec<ToolId>,
    /// Zapis plików jest osobną zgodą użytkownika, nie efektem ubocznym
    /// włączenia narzędzia.
    pub allow_filesystem_write: bool,
    /// Czy R3 w ogóle może wystąpić. Domyślnie nie — ale misja V.I.C.T.O.R.
    /// zakłada, że użytkownik może świadomie włączyć działania krytyczne.
    pub critical_actions: CriticalActionPolicy,
}

/// Polityka wobec działań R3. V.I.C.T.O.R. ma robić wszystko, co rozsądnie
/// bezpieczne — więc R3 nie jest zakazane na zawsze, tylko domyślnie
/// wyłączone i wymaga mocniejszej zgody niż zwykły plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CriticalActionPolicy {
    #[default]
    Blocked,
    RequireStrongConfirmation,
}

impl PolicyConfig {
    pub fn new(allowed_roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            allowed_roots: allowed_roots.into_iter().collect(),
            enabled_tools: Vec::new(),
            allow_filesystem_write: false,
            critical_actions: CriticalActionPolicy::Blocked,
        }
    }

    pub fn with_enabled_tools(mut self, tools: impl IntoIterator<Item = ToolId>) -> Self {
        self.enabled_tools.extend(tools);
        self
    }

    pub fn allow_filesystem_write(mut self, allow: bool) -> Self {
        self.allow_filesystem_write = allow;
        self
    }

    pub fn with_critical_actions(mut self, policy: CriticalActionPolicy) -> Self {
        self.critical_actions = policy;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    pub outcome: Outcome,
    pub risk: RiskLevel,
    pub reasons: Vec<String>,
    /// R3 wymaga zgody `ApprovalKind::Critical`, nie zwykłej.
    pub requires_strong_confirmation: bool,
}

impl PolicyDecision {
    fn allowed(risk: RiskLevel) -> Self {
        Self {
            outcome: if risk.requires_approval() {
                Outcome::ApprovalRequired
            } else {
                Outcome::Allowed
            },
            risk,
            reasons: Vec::new(),
            requires_strong_confirmation: false,
        }
    }

    fn denied(risk: RiskLevel, reason: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Denied,
            risk,
            reasons: vec![reason.into()],
            requires_strong_confirmation: false,
        }
    }

    pub fn is_denied(&self) -> bool {
        self.outcome == Outcome::Denied
    }

    fn merge(mut self, other: PolicyDecision) -> Self {
        self.outcome = self.outcome.max(other.outcome);
        self.risk = self.risk.max(other.risk);
        self.requires_strong_confirmation |= other.requires_strong_confirmation;
        self.reasons.extend(other.reasons);
        self
    }
}

pub struct PolicyEngine {
    guard: PathGuard,
    enabled_tools: HashSet<String>,
    allow_filesystem_write: bool,
    critical_actions: CriticalActionPolicy,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Result<Self, PolicyError> {
        let guard = PathGuard::new(config.allowed_roots)?;
        Ok(Self {
            guard,
            enabled_tools: config
                .enabled_tools
                .into_iter()
                .map(|id| id.as_str().to_string())
                .collect(),
            allow_filesystem_write: config.allow_filesystem_write,
            critical_actions: config.critical_actions,
        })
    }

    pub fn guard(&self) -> &PathGuard {
        &self.guard
    }

    /// Ocena pojedynczego kroku planu względem deklaracji narzędzia.
    pub fn evaluate_step(&self, descriptor: &ToolDescriptor, step: &PlanStep) -> PolicyDecision {
        let risk = descriptor.risk;
        let mut denials: Vec<String> = Vec::new();

        if step.tool != descriptor.id {
            denials.push(format!(
                "krok planu wskazuje `{}`, a oceniane narzędzie to `{}`",
                step.tool, descriptor.id
            ));
        }

        if !self.enabled_tools.contains(descriptor.id.as_str()) {
            denials.push(format!("narzędzie `{}` jest wyłączone", descriptor.id));
        }

        let mut requires_strong_confirmation = false;
        if risk.is_critical() {
            match self.critical_actions {
                CriticalActionPolicy::Blocked => denials.push(format!(
                    "{}: działanie wyłączone w profilu — można je świadomie włączyć w ustawieniach",
                    risk.label()
                )),
                CriticalActionPolicy::RequireStrongConfirmation => {
                    requires_strong_confirmation = true;
                }
            }
        }

        if risk.requires_undo_record() && !descriptor.undo_support {
            denials.push(format!(
                "{} wymaga cofnięcia, a `{}` nie deklaruje `undo_support`",
                risk.label(),
                descriptor.id
            ));
        }

        let paths = match extract_paths(&step.args, &descriptor.path_arguments) {
            Ok(paths) => paths,
            Err(reason) => {
                denials.push(reason);
                Vec::new()
            }
        };

        if !paths.is_empty() {
            let has_read = descriptor.has_capability("filesystem_read");
            let has_write = descriptor.has_capability("filesystem_write");

            if !has_read && !has_write {
                denials.push(format!(
                    "narzędzie `{}` przyjmuje ścieżki, ale nie deklaruje capability plikowej",
                    descriptor.id
                ));
            } else {
                let required_kind = if has_write && risk.requires_approval() {
                    "filesystem_write"
                } else if has_read {
                    "filesystem_read"
                } else {
                    "filesystem_write"
                };

                if required_kind == "filesystem_write" && !self.allow_filesystem_write {
                    denials.push(
                        "zapis plików jest wyłączony w ustawieniach (wymagana osobna zgoda)"
                            .to_string(),
                    );
                }

                let scopes: Vec<&PathScope> = descriptor
                    .capabilities
                    .iter()
                    .filter(|cap| cap.kind() == required_kind)
                    .filter_map(Capability::filesystem_scope)
                    .collect();

                if scopes.is_empty() {
                    denials.push(format!("narzędzie nie deklaruje zakresu `{required_kind}`"));
                }

                for scope in &scopes {
                    if let Err(err) = self.guard.validate(&scope.root) {
                        denials.push(format!(
                            "zakres narzędzia `{}` jest niedozwolony: {err}",
                            scope.root.display()
                        ));
                    }
                }

                for path in &paths {
                    let mut last_error = None;
                    let mut accepted = false;
                    for scope in &scopes {
                        match self.guard.validate_in_scope(path, scope) {
                            Ok(_) => {
                                accepted = true;
                                break;
                            }
                            Err(err) => last_error = Some(err),
                        }
                    }
                    if !accepted {
                        denials.push(match last_error {
                            Some(err) => format!("ścieżka `{}` odrzucona: {err}", path.display()),
                            None => format!(
                                "ścieżka `{}` nie należy do żadnego zakresu narzędzia",
                                path.display()
                            ),
                        });
                    }
                }
            }
        }

        if denials.is_empty() {
            let mut decision = PolicyDecision::allowed(risk);
            decision.requires_strong_confirmation = requires_strong_confirmation;
            decision
        } else {
            PolicyDecision {
                outcome: Outcome::Denied,
                risk,
                reasons: denials,
                requires_strong_confirmation: false,
            }
        }
    }

    /// Ocena całego planu. Każdy krok musi mieć zarejestrowane narzędzie.
    pub fn evaluate_plan(&self, registry: &[ToolDescriptor], plan: &Plan) -> PolicyDecision {
        let mut decision = PolicyDecision {
            outcome: Outcome::Allowed,
            risk: plan.risk,
            reasons: Vec::new(),
            requires_strong_confirmation: false,
        };

        for step in &plan.steps {
            match registry
                .iter()
                .find(|descriptor| descriptor.id == step.tool)
            {
                Some(descriptor) => {
                    decision = decision.merge(self.evaluate_step(descriptor, step));
                }
                None => {
                    decision = decision.merge(PolicyDecision::denied(
                        plan.risk,
                        format!(
                            "krok planu używa niezarejestrowanego narzędzia `{}`",
                            step.tool
                        ),
                    ));
                }
            }
        }

        decision
    }

    /// Weryfikacja zgody. Plany R0 nie wymagają zgody. R3 wymaga zgody
    /// krytycznej (`ApprovalKind::Critical`) i włączonej polityki R3.
    pub fn require_approval(
        &self,
        plan: &Plan,
        approval: Option<&Approval>,
        now: UnixMs,
    ) -> Result<(), PolicyError> {
        if plan.risk.is_critical() {
            if self.critical_actions == CriticalActionPolicy::Blocked {
                return Err(PolicyError::CriticalActionsDisabled);
            }
            let approval = approval.ok_or(PolicyError::MissingApproval)?;
            approval.verify(plan, now)?;
            if approval.kind != ApprovalKind::Critical {
                return Err(PolicyError::WeakApproval);
            }
            return Ok(());
        }

        if !plan.requires_approval() {
            return Ok(());
        }
        let approval = approval.ok_or(PolicyError::MissingApproval)?;
        approval.verify(plan, now).map_err(PolicyError::from)
    }
}

/// Wyciąga ścieżki z argumentów wyłącznie po nazwach zadeklarowanych przez
/// narzędzie. Nic nie jest zgadywane na podstawie kształtu JSON-a.
fn extract_paths(args: &Value, keys: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    if keys.is_empty() {
        return Ok(paths);
    }

    let Some(object) = args.as_object() else {
        return Err("argumenty narzędzia muszą być obiektem JSON".to_string());
    };

    for key in keys {
        match object.get(key) {
            None => continue,
            Some(Value::String(value)) => paths.push(PathBuf::from(value)),
            Some(Value::Array(items)) => {
                for item in items {
                    match item.as_str() {
                        Some(value) => paths.push(PathBuf::from(value)),
                        None => {
                            return Err(format!("pole `{key}` musi być listą stringów (ścieżek)"))
                        }
                    }
                }
            }
            Some(_) => {
                return Err(format!(
                    "pole `{key}` musi być stringiem lub listą stringów"
                ))
            }
        }
    }

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use std::path::Path;

    fn read_tool(scope: &Path) -> ToolDescriptor {
        ToolDescriptor {
            id: ToolId::parse("files.scan").unwrap(),
            schema_version: 1,
            description: "Skan katalogu".into(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            capabilities: vec![Capability::FilesystemRead {
                scope: PathScope::new(scope),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 30_000,
            undo_support: false,
            path_arguments: vec!["path".into()],
        }
    }

    fn write_tool(scope: &Path) -> ToolDescriptor {
        ToolDescriptor {
            id: ToolId::parse("files.move_to_staging").unwrap(),
            schema_version: 1,
            description: "Przenieś pliki do staging".into(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            capabilities: vec![Capability::FilesystemWrite {
                scope: PathScope::new(scope),
            }],
            risk: RiskLevel::R1,
            default_timeout_ms: 30_000,
            undo_support: true,
            path_arguments: vec!["path".into()],
        }
    }

    fn step_for(descriptor: &ToolDescriptor, path: &Path) -> PlanStep {
        PlanStep::new(
            descriptor.id.clone(),
            "krok testowy",
            json!({ "path": path }),
            descriptor.risk,
        )
    }

    fn engine(root: &Path, descriptors: &[ToolDescriptor], allow_write: bool) -> PolicyEngine {
        let config = PolicyConfig::new([root.to_path_buf()])
            .with_enabled_tools(descriptors.iter().map(|d| d.id.clone()))
            .allow_filesystem_write(allow_write);
        PolicyEngine::new(config).unwrap()
    }

    #[test]
    fn r0_read_inside_root_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let descriptor = read_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), false);

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::Allowed);
        assert!(decision.reasons.is_empty());
    }

    #[test]
    fn r0_read_outside_root_is_denied() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let descriptor = read_tool(root.path());
        let engine = engine(root.path(), std::slice::from_ref(&descriptor), false);

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::Denied);
        assert!(!decision.reasons.is_empty());
    }

    #[test]
    fn r1_write_requires_approval() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let descriptor = write_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), true);

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::ApprovalRequired);
    }

    #[test]
    fn write_is_denied_when_disabled_in_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let descriptor = write_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), false);

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::Denied);
        assert!(decision.reasons.iter().any(|r| r.contains("zapis")));
    }

    #[test]
    fn r1_without_undo_support_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut descriptor = write_tool(dir.path());
        descriptor.undo_support = false;
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), true);

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::Denied);
        assert!(decision.reasons.iter().any(|r| r.contains("undo_support")));
    }

    #[test]
    fn r3_is_denied_in_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut descriptor = write_tool(dir.path());
        descriptor.risk = RiskLevel::R3;
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), true);

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::Denied);
    }

    #[test]
    fn disabled_tool_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let descriptor = read_tool(dir.path());
        let config = PolicyConfig::new([dir.path().to_path_buf()]);
        let engine = PolicyEngine::new(config).unwrap();

        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &file));
        assert_eq!(decision.outcome, Outcome::Denied);
        assert!(decision.reasons.iter().any(|r| r.contains("wyłączone")));
    }

    #[test]
    fn path_traversal_in_args_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = read_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), false);
        let step = PlanStep::new(
            descriptor.id.clone(),
            "traversal",
            json!({"path": dir.path().join("..").join("etc")}),
            RiskLevel::R0,
        );

        let decision = engine.evaluate_step(&descriptor, &step);
        assert_eq!(decision.outcome, Outcome::Denied);
    }

    #[test]
    fn symlink_escape_in_args_is_denied() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("sekret");
        std::fs::write(&secret, b"x").unwrap();
        let link = root.path().join("link");
        symlink(&secret, &link).unwrap();

        let descriptor = read_tool(root.path());
        let engine = engine(root.path(), std::slice::from_ref(&descriptor), false);
        let decision = engine.evaluate_step(&descriptor, &step_for(&descriptor, &link));
        assert_eq!(decision.outcome, Outcome::Denied);
        assert!(decision.reasons.iter().any(|r| r.contains("symlink")));
    }

    #[test]
    fn path_args_without_filesystem_capability_are_denied() {
        let dir = tempfile::tempdir().unwrap();
        let mut descriptor = read_tool(dir.path());
        descriptor.capabilities.clear();
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), false);

        let decision = engine.evaluate_step(
            &descriptor,
            &step_for(&descriptor, &dir.path().join("a.txt")),
        );
        assert_eq!(decision.outcome, Outcome::Denied);
        assert!(decision.reasons.iter().any(|r| r.contains("capability")));
    }

    #[test]
    fn scope_outside_allowed_roots_is_denied() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut descriptor = read_tool(outside.path());
        descriptor.id = ToolId::parse("files.other").unwrap();
        let engine = engine(root.path(), std::slice::from_ref(&descriptor), false);

        let decision = engine.evaluate_step(
            &descriptor,
            &step_for(&descriptor, &outside.path().join("a.txt")),
        );
        assert_eq!(decision.outcome, Outcome::Denied);
    }

    #[test]
    fn plan_uses_worst_outcome_across_steps() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let reader = read_tool(dir.path());
        let writer = write_tool(dir.path());
        let engine = engine(dir.path(), &[reader.clone(), writer.clone()], true);

        let plan = Plan::new(
            "t1",
            1,
            "cleanup",
            vec![step_for(&reader, &file), step_for(&writer, &file)],
        )
        .unwrap();

        let decision = engine.evaluate_plan(&[reader, writer], &plan);
        assert_eq!(decision.outcome, Outcome::ApprovalRequired);
    }

    #[test]
    fn plan_with_unknown_tool_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let reader = read_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&reader), false);
        let ghost = ToolId::parse("files.ghost").unwrap();
        let plan = Plan::new(
            "t1",
            1,
            "x",
            vec![PlanStep::new(ghost, "nieznane", json!({}), RiskLevel::R0)],
        )
        .unwrap();

        let decision = engine.evaluate_plan(&[reader], &plan);
        assert_eq!(decision.outcome, Outcome::Denied);
    }

    #[test]
    fn approval_is_required_and_verified() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let writer = write_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&writer), true);

        let plan = Plan::new("t1", 1, "cleanup", vec![step_for(&writer, &file)]).unwrap();

        assert_eq!(
            engine.require_approval(&plan, None, 1_000).unwrap_err(),
            PolicyError::MissingApproval
        );

        let approval = plan.approve(1_000).unwrap();
        assert!(engine
            .require_approval(&plan, Some(&approval), 1_001)
            .is_ok());

        let mut changed = plan.clone();
        changed.steps[0].args = json!({"path": "/inny"});
        assert!(matches!(
            engine.require_approval(&changed, Some(&approval), 1_001),
            Err(PolicyError::InvalidApproval(_))
        ));
    }

    #[test]
    fn r0_plan_needs_no_approval() {
        let dir = tempfile::tempdir().unwrap();
        let reader = read_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&reader), false);
        let plan = Plan::new(
            "t1",
            1,
            "scan",
            vec![step_for(&reader, &dir.path().join("a.txt"))],
        )
        .unwrap();
        assert!(engine.require_approval(&plan, None, 0).is_ok());
    }

    fn critical_engine(root: &Path, descriptor: &ToolDescriptor) -> PolicyEngine {
        let config = PolicyConfig::new([root.to_path_buf()])
            .with_enabled_tools([descriptor.id.clone()])
            .allow_filesystem_write(true)
            .with_critical_actions(CriticalActionPolicy::RequireStrongConfirmation);
        PolicyEngine::new(config).unwrap()
    }

    #[test]
    fn r3_needs_critical_approval_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut descriptor = write_tool(dir.path());
        descriptor.risk = RiskLevel::R3;
        let engine = critical_engine(dir.path(), &descriptor);

        let step = step_for(&descriptor, &file);
        let decision = engine.evaluate_step(&descriptor, &step);
        assert_eq!(decision.outcome, Outcome::ApprovalRequired);
        assert!(decision.requires_strong_confirmation);

        let plan = Plan::new("t1", 1, "delete", vec![step]).unwrap();

        assert_eq!(
            engine.require_approval(&plan, None, 1_000).unwrap_err(),
            PolicyError::MissingApproval
        );

        let standard = plan.approve(1_000).unwrap();
        assert_eq!(
            engine
                .require_approval(&plan, Some(&standard), 1_001)
                .unwrap_err(),
            PolicyError::WeakApproval
        );

        let critical = plan.approve_critical(1_000).unwrap();
        assert!(engine
            .require_approval(&plan, Some(&critical), 1_001)
            .is_ok());
    }

    #[test]
    fn r3_is_blocked_even_with_critical_approval() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut descriptor = write_tool(dir.path());
        descriptor.risk = RiskLevel::R3;
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), true);

        let plan = Plan::new("t1", 1, "delete", vec![step_for(&descriptor, &file)]).unwrap();
        let critical = plan.approve_critical(1_000).unwrap();

        assert_eq!(
            engine
                .require_approval(&plan, Some(&critical), 1_001)
                .unwrap_err(),
            PolicyError::CriticalActionsDisabled
        );
    }

    #[test]
    fn standard_plan_does_not_accept_critical_only_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let descriptor = write_tool(dir.path());
        let engine = engine(dir.path(), std::slice::from_ref(&descriptor), true);
        let plan = Plan::new("t1", 1, "move", vec![step_for(&descriptor, &file)]).unwrap();

        let critical = plan.approve_critical(1_000).unwrap();
        assert!(engine
            .require_approval(&plan, Some(&critical), 1_001)
            .is_ok());
    }
}
