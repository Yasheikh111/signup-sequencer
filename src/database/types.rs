use chrono::{DateTime, Utc};

use crate::identity_tree::{Hash, Status};

pub struct UnprocessedCommitment {
    pub commitment:    Hash,
    pub status:        Status,
    pub created_at:    DateTime<Utc>,
    pub processed_at:  Option<DateTime<Utc>>,
    pub error_message: Option<String>,
}
