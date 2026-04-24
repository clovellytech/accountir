use chrono::NaiveDate;
use rusqlite::Connection;

/// A detected recurring subscription.
#[derive(Debug, Clone)]
pub struct DetectedSubscription {
    pub memo: String,
    pub frequency: SubscriptionFrequency,
    pub avg_amount: i64,
    pub occurrence_count: u32,
    pub last_date: NaiveDate,
    pub entry_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionFrequency {
    Weekly,
    Biweekly,
    Monthly,
    Quarterly,
    Annual,
}

impl SubscriptionFrequency {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Weekly => "Weekly",
            Self::Biweekly => "Biweekly",
            Self::Monthly => "Monthly",
            Self::Quarterly => "Quarterly",
            Self::Annual => "Annual",
        }
    }

    fn expected_days(&self) -> f64 {
        match self {
            Self::Weekly => 7.0,
            Self::Biweekly => 14.0,
            Self::Monthly => 30.4,
            Self::Quarterly => 91.3,
            Self::Annual => 365.25,
        }
    }
}

const FREQUENCIES: &[SubscriptionFrequency] = &[
    SubscriptionFrequency::Weekly,
    SubscriptionFrequency::Biweekly,
    SubscriptionFrequency::Monthly,
    SubscriptionFrequency::Quarterly,
    SubscriptionFrequency::Annual,
];

/// Find the uncategorized account ID, if it exists.
fn find_uncategorized_id(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT id FROM accounts WHERE LOWER(name) = 'uncategorized' LIMIT 1",
        [],
        |row| row.get(0),
    )
    .ok()
}

/// Detect subscriptions from recurring transactions in the Uncategorized account.
pub fn detect_subscriptions(conn: &Connection) -> Vec<DetectedSubscription> {
    let uncategorized_id = match find_uncategorized_id(conn) {
        Some(id) => id,
        None => return Vec::new(),
    };

    // Get all non-voided entries that have a line on the uncategorized account,
    // grouped by normalized memo. We want the OTHER account's line amount
    // (the actual charge), not the uncategorized offset line.
    let mut stmt = match conn.prepare(
        "SELECT je.id, je.date, je.memo, jl.amount
         FROM journal_entries je
         JOIN journal_lines jl ON jl.entry_id = je.id
         WHERE je.is_void = 0
           AND jl.account_id != ?1
           AND je.id IN (
               SELECT entry_id FROM journal_lines WHERE account_id = ?1
           )
         ORDER BY LOWER(je.memo), je.date",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    struct RawTxn {
        entry_id: String,
        date: NaiveDate,
        memo: String,
        amount: i64,
    }

    let rows: Vec<RawTxn> = stmt
        .query_map([&uncategorized_id], |row| {
            let date_str: String = row.get(1)?;
            let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                .unwrap_or_else(|_| NaiveDate::from_ymd_opt(2000, 1, 1).unwrap());
            Ok(RawTxn {
                entry_id: row.get(0)?,
                date,
                memo: row.get(2)?,
                amount: row.get(3)?,
            })
        })
        .ok()
        .map(|r| r.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    // Group by normalized memo
    let mut groups: std::collections::HashMap<String, Vec<&RawTxn>> =
        std::collections::HashMap::new();
    for txn in &rows {
        let key = txn.memo.trim().to_lowercase();
        groups.entry(key).or_default().push(txn);
    }

    let mut subscriptions = Vec::new();

    for txns in groups.values() {
        if txns.len() < 2 {
            continue;
        }

        // Sort by date
        let mut sorted: Vec<&&RawTxn> = txns.iter().collect();
        sorted.sort_by_key(|t| t.date);

        // Calculate intervals between consecutive dates
        let intervals: Vec<i64> = sorted
            .windows(2)
            .map(|w| (w[1].date - w[0].date).num_days())
            .collect();

        if intervals.is_empty() {
            continue;
        }

        // Find the best matching frequency
        let avg_interval = intervals.iter().sum::<i64>() as f64 / intervals.len() as f64;

        let best = FREQUENCIES.iter().min_by(|a, b| {
            let err_a = (avg_interval - a.expected_days()).abs();
            let err_b = (avg_interval - b.expected_days()).abs();
            err_a.partial_cmp(&err_b).unwrap()
        });

        let frequency = match best {
            Some(f) => *f,
            None => continue,
        };

        // Check consistency: most intervals should be close to expected
        let expected = frequency.expected_days();
        let tolerance = expected * 0.35;
        let consistent_count = intervals
            .iter()
            .filter(|&&d| (d as f64 - expected).abs() <= tolerance)
            .count();

        // Require at least 60% of intervals to be consistent
        if consistent_count * 100 / intervals.len() < 60 {
            continue;
        }

        let avg_amount = sorted.iter().map(|t| t.amount).sum::<i64>() / sorted.len() as i64;
        let last_date = sorted.last().unwrap().date;
        let entry_ids: Vec<String> = sorted.iter().map(|t| t.entry_id.clone()).collect();

        subscriptions.push(DetectedSubscription {
            memo: sorted[0].memo.clone(),
            frequency,
            avg_amount,
            occurrence_count: sorted.len() as u32,
            last_date,
            entry_ids,
        });
    }

    // Sort by frequency (monthly first), then by amount descending
    subscriptions.sort_by(|a, b| {
        let freq_order = |f: &SubscriptionFrequency| match f {
            SubscriptionFrequency::Monthly => 0,
            SubscriptionFrequency::Weekly => 1,
            SubscriptionFrequency::Biweekly => 2,
            SubscriptionFrequency::Quarterly => 3,
            SubscriptionFrequency::Annual => 4,
        };
        freq_order(&a.frequency)
            .cmp(&freq_order(&b.frequency))
            .then(b.avg_amount.abs().cmp(&a.avg_amount.abs()))
    });

    subscriptions
}
