use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::capability::RiskLevel;
use crate::error::DomainError;
use crate::tool::ToolId;
use crate::UnixMs;

/// Domyślny czas życia zgody: 5 minut. Zgoda jest krótkotrwała i powiązana
/// z hashem dokładnego planu (§13).
pub const DEFAULT_APPROVAL_TTL_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub tool: ToolId,
    pub description: String,
    pub args: Value,
    pub risk: RiskLevel,
}

impl PlanStep {
    pub fn new(tool: ToolId, description: impl Into<String>, args: Value, risk: RiskLevel) -> Self {
        Self {
            tool,
            description: description.into(),
            args,
            risk,
        }
    }
}

/// Hash dokładnego planu. Zmiana argumentu, kolejności kroków albo wersji
/// unieważnia zgodę.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanHash(String);

impl PlanHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub task_id: String,
    pub version: u32,
    pub intent: String,
    pub steps: Vec<PlanStep>,
    /// Najwyższe ryzyko spośród kroków; wyliczane w `Plan::new`.
    pub risk: RiskLevel,
}

impl Plan {
    pub fn new(
        task_id: impl Into<String>,
        version: u32,
        intent: impl Into<String>,
        steps: Vec<PlanStep>,
    ) -> Result<Self, DomainError> {
        if steps.is_empty() {
            return Err(DomainError::EmptyPlan);
        }
        let risk = steps
            .iter()
            .map(|step| step.risk)
            .max()
            .expect("steps nie jest puste");
        Ok(Self {
            task_id: task_id.into(),
            version,
            intent: intent.into(),
            steps,
            risk,
        })
    }

    pub fn requires_approval(&self) -> bool {
        self.risk.requires_approval()
    }

    /// SHA-256 po deterministycznej serializacji planu.
    ///
    /// `serde_json::Value` używa mapy sortowanej po kluczu, więc kolejność
    /// pól w argumentach nie wpływa na hash.
    pub fn hash(&self) -> PlanHash {
        let bytes = serde_json::to_vec(self).expect("Plan jest serializowalny");
        let digest = Sha256::digest(&bytes);
        let mut hex = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            write!(hex, "{byte:02x}").expect("zapis do String nie może się nie udać");
        }
        PlanHash(hex)
    }

    pub fn approve(&self, now: UnixMs) -> Result<Approval, DomainError> {
        self.approve_with_ttl(now, DEFAULT_APPROVAL_TTL_MS)
    }

    pub fn approve_with_ttl(&self, now: UnixMs, ttl_ms: u64) -> Result<Approval, DomainError> {
        self.build_approval(now, ttl_ms, ApprovalKind::Standard)
    }

    /// Zgoda na działanie krytyczne (R3): trwałe usunięcie, zmiana systemu,
    /// instalacja nieznanego oprogramowania, wysyłka danych poza komputer.
    /// Różni się od zwykłej zgody tym, że UI musi pokazać osobny, mocniejszy
    /// ekran potwierdzenia — sam hash planu nie wystarcza.
    pub fn approve_critical(&self, now: UnixMs) -> Result<Approval, DomainError> {
        self.build_approval(now, DEFAULT_APPROVAL_TTL_MS, ApprovalKind::Critical)
    }

    pub fn approve_critical_with_ttl(
        &self,
        now: UnixMs,
        ttl_ms: u64,
    ) -> Result<Approval, DomainError> {
        self.build_approval(now, ttl_ms, ApprovalKind::Critical)
    }

    fn build_approval(
        &self,
        now: UnixMs,
        ttl_ms: u64,
        kind: ApprovalKind,
    ) -> Result<Approval, DomainError> {
        if ttl_ms == 0 {
            return Err(DomainError::InvalidApprovalWindow);
        }
        let expires_at = now
            .checked_add(ttl_ms)
            .ok_or(DomainError::InvalidApprovalWindow)?;
        Ok(Approval {
            plan_hash: self.hash(),
            granted_at_unix_ms: now,
            expires_at_unix_ms: expires_at,
            kind,
        })
    }
}

/// Rodzaj zgody. `Critical` jest wymagana dla R3 i nie może być podstawiona
/// przez zwykłe kliknięcie „Zatwierdź" w widoku planu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    #[default]
    Standard,
    Critical,
}

/// Zgoda użytkownika na dokładny, niezmienny plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub plan_hash: PlanHash,
    pub granted_at_unix_ms: UnixMs,
    pub expires_at_unix_ms: UnixMs,
    #[serde(default)]
    pub kind: ApprovalKind,
}

impl Approval {
    pub fn is_expired(&self, now: UnixMs) -> bool {
        now >= self.expires_at_unix_ms
    }

    pub fn is_critical(&self) -> bool {
        self.kind == ApprovalKind::Critical
    }

    /// Zgoda jest ważna tylko dla planu o identycznym hashu i przed upływem TTL.
    pub fn verify(&self, plan: &Plan, now: UnixMs) -> Result<(), DomainError> {
        if self.plan_hash != plan.hash() {
            return Err(DomainError::ApprovalMismatch);
        }
        if self.is_expired(now) {
            return Err(DomainError::ApprovalExpired);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolId {
        ToolId::parse(name).unwrap()
    }

    fn sample_plan() -> Plan {
        Plan::new(
            "task-1",
            1,
            "inspect_storage",
            vec![
                PlanStep::new(
                    tool("files.scan"),
                    "Skan katalogu projektów",
                    json!({"path": "/home/jaro/projekty"}),
                    RiskLevel::R0,
                ),
                PlanStep::new(
                    tool("files.move_to_staging"),
                    "Przenieś stare pliki do staging",
                    json!({"path": "/home/jaro/projekty/stare"}),
                    RiskLevel::R1,
                ),
            ],
        )
        .unwrap()
    }

    #[test]
    fn plan_risk_is_max_of_steps() {
        assert_eq!(sample_plan().risk, RiskLevel::R1);
    }

    #[test]
    fn empty_plan_is_rejected() {
        let err = Plan::new("task-1", 1, "noop", vec![]).unwrap_err();
        assert_eq!(err, DomainError::EmptyPlan);
    }

    #[test]
    fn hash_is_stable_for_identical_plans() {
        assert_eq!(sample_plan().hash(), sample_plan().hash());
    }

    #[test]
    fn hash_ignores_argument_key_order() {
        let a = Plan::new(
            "t",
            1,
            "i",
            vec![PlanStep::new(
                tool("files.scan"),
                "d",
                json!({"path": "/x", "depth": 2}),
                RiskLevel::R0,
            )],
        )
        .unwrap();
        let b = Plan::new(
            "t",
            1,
            "i",
            vec![PlanStep::new(
                tool("files.scan"),
                "d",
                json!({"depth": 2, "path": "/x"}),
                RiskLevel::R0,
            )],
        )
        .unwrap();
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn hash_changes_when_argument_changes() {
        let mut plan = sample_plan();
        let before = plan.hash();
        plan.steps[0].args = json!({"path": "/home/jaro/inny"});
        assert_ne!(before, plan.hash());
    }

    #[test]
    fn hash_changes_when_step_order_changes() {
        let plan = sample_plan();
        let mut reversed = plan.clone();
        reversed.steps.reverse();
        assert_ne!(plan.hash(), reversed.hash());
    }

    #[test]
    fn approval_verifies_for_unchanged_plan() {
        let plan = sample_plan();
        let approval = plan.approve(1_000).unwrap();
        assert!(approval.verify(&plan, 1_500).is_ok());
    }

    #[test]
    fn approval_is_rejected_after_plan_change() {
        let plan = sample_plan();
        let approval = plan.approve(1_000).unwrap();
        let mut changed = plan.clone();
        changed.steps[1].args = json!({"path": "/home/jaro/wszystko"});
        assert_eq!(
            approval.verify(&changed, 1_500).unwrap_err(),
            DomainError::ApprovalMismatch
        );
    }

    #[test]
    fn approval_expires() {
        let plan = sample_plan();
        let approval = plan.approve_with_ttl(1_000, 500).unwrap();
        assert!(approval.verify(&plan, 1_499).is_ok());
        assert_eq!(
            approval.verify(&plan, 1_500).unwrap_err(),
            DomainError::ApprovalExpired
        );
    }

    #[test]
    fn zero_ttl_is_rejected() {
        let plan = sample_plan();
        assert_eq!(
            plan.approve_with_ttl(1_000, 0).unwrap_err(),
            DomainError::InvalidApprovalWindow
        );
    }

    #[test]
    fn ttl_overflow_is_rejected() {
        let plan = sample_plan();
        assert_eq!(
            plan.approve_with_ttl(1_000, u64::MAX).unwrap_err(),
            DomainError::InvalidApprovalWindow
        );
    }
}
