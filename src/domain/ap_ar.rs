use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Payment terms attached to a bill or invoice
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaymentTerms {
    DueOnReceipt,
    Net { days: u32 },
}

impl PaymentTerms {
    /// Compute the due date given an issue/receive date
    pub fn due_date(&self, issue_date: NaiveDate) -> NaiveDate {
        match self {
            PaymentTerms::DueOnReceipt => issue_date,
            PaymentTerms::Net { days } => issue_date + chrono::Duration::days(*days as i64),
        }
    }

    /// Human-readable label
    pub fn label(&self) -> String {
        match self {
            PaymentTerms::DueOnReceipt => "Due on Receipt".to_string(),
            PaymentTerms::Net { days } => format!("Net {}", days),
        }
    }

    /// Parse from a CLI-friendly string like "net30", "due-on-receipt", "60"
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().replace('-', "").replace(' ', "").as_str() {
            "dueonreceipt" | "immediate" | "due" => PaymentTerms::DueOnReceipt,
            "net30" => PaymentTerms::Net { days: 30 },
            "net60" => PaymentTerms::Net { days: 60 },
            "net90" => PaymentTerms::Net { days: 90 },
            other => other
                .parse::<u32>()
                .map(|d| PaymentTerms::Net { days: d })
                .unwrap_or(PaymentTerms::Net { days: 30 }),
        }
    }
}
