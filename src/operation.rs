use crate::compress::ArchivePlan;
use crate::table::Table;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static PLAN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedOperation {
    Archive(ArchivePlan),
}

/// A typed, immutable description of intended side effects. The plan owns
/// the exact FileRecords observed during planning; applying never rescans to
/// construct a replacement plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPlan {
    pub id: String,
    pub operation: PlannedOperation,
    pub force: bool,
}

impl OperationPlan {
    pub fn archive(plan: ArchivePlan, force: bool) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let sequence = PLAN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            id: format!("plan-{nanos:x}-{sequence:x}"),
            operation: PlannedOperation::Archive(plan),
            force,
        }
    }

    pub fn to_table(&self) -> Table {
        let mut table = match &self.operation {
            PlannedOperation::Archive(plan) => plan.to_table(),
        };
        for row in &mut table.rows {
            row.insert(0, ("plan_id".to_string(), self.id.clone()));
        }
        table
    }

    pub async fn apply(&self) -> Result<String, String> {
        match &self.operation {
            PlannedOperation::Archive(plan) => {
                crate::compress::apply_archive_plan(plan, self.force).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_plan_preview_carries_its_typed_plan_id() {
        let plan = OperationPlan::archive(ArchivePlan { items: Vec::new() }, false);
        let table = plan.to_table();
        // Empty operation plans are valid values even though there are no
        // rows on which to repeat the ID; identity remains on the typed value.
        assert!(plan.id.starts_with("plan-"));
        assert!(table.rows.is_empty());
    }
}
