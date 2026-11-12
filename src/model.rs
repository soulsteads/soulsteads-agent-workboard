use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

macro_rules! choice {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
choice!(IssueKind {
    Epic,
    Story,
    Task,
    Bug,
    Subtask,
    Spike
});
choice!(State {
    Backlog,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Done,
    Canceled
});
choice!(Priority {
    Critical,
    High,
    Medium,
    Low
});
choice!(LinkKind {
    Blocks,
    RelatesTo,
    Duplicates
});
choice!(SprintState {
    Planned,
    Active,
    Completed
});

impl State {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Canceled)
    }
    pub fn permits(self, target: Self) -> bool {
        use State::*;
        matches!(
            (self, target),
            (Backlog, Ready | Canceled)
                | (Ready, Backlog | InProgress | Blocked | Canceled)
                | (InProgress, Ready | Blocked | InReview | Done | Canceled)
                | (Blocked, Ready | InProgress | Backlog | Canceled)
                | (InReview, InProgress | Blocked | Done | Canceled)
                | (Done | Canceled, Backlog)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub active: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiKey {
    pub id: String,
    pub agent: String,
    pub label: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub key: String,
    pub name: String,
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Lease {
    pub token: String,
    pub expires_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Issue {
    pub key: String,
    pub project: String,
    pub kind: IssueKind,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub state: State,
    pub priority: Priority,
    pub assignee: Option<String>,
    pub parent: Option<String>,
    pub sprint: Option<String>,
    pub labels: Vec<String>,
    pub estimate: Option<f64>,
    pub metadata: BTreeMap<String, Value>,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub lease: Option<Lease>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Link {
    pub source: String,
    pub target: String,
    pub kind: LinkKind,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Comment {
    pub id: i64,
    pub issue: String,
    pub actor: String,
    pub body: String,
    pub created_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub id: i64,
    pub issue: Option<String>,
    pub actor: String,
    pub action: String,
    pub data: Value,
    pub created_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Sprint {
    pub id: String,
    pub project: String,
    pub name: String,
    pub goal: String,
    pub state: SprintState,
}

/// A push subscription. The signing secret is stored apart from this document and
/// is returned exactly once, when the webhook is created.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Webhook {
    pub id: String,
    pub label: String,
    pub url: String,
    pub project: Option<String>,
    /// Event actions to deliver; empty means every action.
    pub actions: Vec<String>,
    pub active: bool,
    /// Id of the last event already handled; delivery resumes after it.
    pub cursor: i64,
    pub failures: u32,
    pub last_error: Option<String>,
    pub last_delivered_at: Option<i64>,
    pub created_at: i64,
}

/// Leases are displayed by expiry, never by their worker fencing credential.
pub fn redact_leases(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Object(lease)) = object.get_mut("lease") {
                lease.remove("token");
            }
            for child in object.values_mut() {
                redact_leases(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                redact_leases(child);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IssueFilter {
    pub project: Option<String>,
    pub kind: Option<IssueKind>,
    pub state: Option<State>,
    pub assignee: Option<String>,
    pub unassigned: bool,
    pub parent: Option<String>,
    pub sprint: Option<String>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub actionable: bool,
    pub offset: usize,
    pub limit: Option<usize>,
}

/// Every operation is available identically through the CLI and MCP.
/// Revisions are mandatory for editing an existing issue to prevent lost updates.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    AgentRegister {
        id: String,
        name: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    AgentSetActive {
        id: String,
        active: bool,
    },
    AgentList {},
    /// Local administration only; the secret is returned once and stored hashed.
    ApiKeyCreate {
        agent: String,
        label: String,
    },
    ApiKeyList {
        #[serde(default)]
        agent: Option<String>,
    },
    ApiKeyRevoke {
        id: String,
    },
    ProjectCreate {
        key: String,
        name: String,
        #[serde(default)]
        description: String,
    },
    ProjectList {},
    IssueCreate {
        project: String,
        kind: IssueKind,
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        acceptance_criteria: Vec<String>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        sprint: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        estimate: Option<f64>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default)]
        metadata: BTreeMap<String, Value>,
    },
    IssueGet {
        key: String,
    },
    IssueList {
        #[serde(default)]
        filter: IssueFilter,
    },
    IssueUpdate {
        key: String,
        expected_revision: i64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        acceptance_criteria: Option<Vec<String>>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default)]
        labels: Option<Vec<String>>,
        #[serde(default)]
        estimate: Option<f64>,
        #[serde(default)]
        clear_estimate: bool,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        clear_parent: bool,
        #[serde(default)]
        sprint: Option<String>,
        #[serde(default)]
        clear_sprint: bool,
        #[serde(default)]
        metadata: Option<BTreeMap<String, Value>>,
        #[serde(default)]
        lease_token: Option<String>,
    },
    IssueAssign {
        key: String,
        expected_revision: i64,
        assignee: Option<String>,
    },
    IssueTransition {
        key: String,
        expected_revision: i64,
        state: State,
        #[serde(default)]
        lease_token: Option<String>,
    },
    IssueClaim {
        project: String,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        lease_seconds: Option<i64>,
    },
    IssueHeartbeat {
        key: String,
        lease_token: String,
        #[serde(default)]
        lease_seconds: Option<i64>,
    },
    IssueRelease {
        key: String,
        lease_token: String,
    },
    LinkAdd {
        source: String,
        target: String,
        kind: LinkKind,
    },
    LinkRemove {
        source: String,
        target: String,
        kind: LinkKind,
    },
    CommentAdd {
        key: String,
        body: String,
    },
    CommentList {
        key: String,
        #[serde(default)]
        after: Option<i64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Events {
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        after: Option<i64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Board {
        project: String,
        #[serde(default)]
        sprint: Option<String>,
    },
    EpicProgress {
        key: String,
    },
    SprintCreate {
        project: String,
        name: String,
        #[serde(default)]
        goal: String,
    },
    SprintList {
        project: String,
    },
    SprintTransition {
        id: String,
        state: SprintState,
    },
    Workflow {},
    /// Operator only. Subscribe a URL to board events; the signing secret is shown once.
    WebhookCreate {
        url: String,
        label: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        actions: Vec<String>,
    },
    /// Operator only. List webhooks with delivery health; never secrets or full URLs.
    WebhookList {},
    /// Operator only. Pause or resume a webhook; resuming clears its failure count.
    WebhookSetActive {
        id: String,
        active: bool,
    },
    /// Operator only. Permanently remove a webhook.
    WebhookDelete {
        id: String,
    },
}

impl Command {
    /// Administration that handles secrets or outbound integrations. It is limited
    /// to trusted local operator sessions and never offered to key-authenticated agents.
    pub fn is_operator_only(&self) -> bool {
        matches!(
            self,
            Self::ApiKeyCreate { .. }
                | Self::ApiKeyList { .. }
                | Self::ApiKeyRevoke { .. }
                | Self::WebhookCreate { .. }
                | Self::WebhookList {}
                | Self::WebhookSetActive { .. }
                | Self::WebhookDelete { .. }
        )
    }
}
