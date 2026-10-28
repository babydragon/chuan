use serde::{Deserialize, Serialize};

use crate::ids::{ConversationId, RunId, Seq, Timestamp, TurnId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    RunStarted,
    TurnStarted,
    TurnFinished,
    MsgAssistant,
    MsgToolResult,
    MsgSynthetic,
    ToolCallStarted,
    ToolCallDispatched,
    ToolCallFinished,
    ToolCallAbandoned,
    ApprovalRequested,
    ApprovalCardSent,
    ApprovalResolved,
    TaskEnqueued,
    TaskLeased,
    TaskFinished,
    RunFinished,
    AgentSpawned,
    AgentConcluded,
    UsageRecorded,
    ArtifactCreated,
    CtxCompacted,
    DocUpserted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub conv: ConversationId,
    pub seq: Seq,
    pub run: RunId,
    pub turn: Option<TurnId>,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub meta: Option<serde_json::Value>,
    pub dedupe_key: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug)]
pub struct NewEvent {
    pub conv: ConversationId,
    pub run: RunId,
    pub turn: Option<TurnId>,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub dedupe_key: Option<String>,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        use EventKind::*;
        match self {
            RunStarted => "run.started",
            TurnStarted => "turn.started",
            TurnFinished => "turn.finished",
            MsgAssistant => "msg.assistant",
            MsgToolResult => "msg.tool_result",
            MsgSynthetic => "msg.synthetic",
            ToolCallStarted => "tool.call_started",
            ToolCallDispatched => "tool.call_dispatched",
            ToolCallFinished => "tool.call_finished",
            ToolCallAbandoned => "tool.call_abandoned",
            ApprovalRequested => "approval.requested",
            ApprovalCardSent => "approval.card_sent",
            ApprovalResolved => "approval.resolved",
            TaskEnqueued => "task.enqueued",
            TaskLeased => "task.leased",
            TaskFinished => "task.finished",
            RunFinished => "run.finished",
            AgentSpawned => "agent.spawned",
            AgentConcluded => "agent.concluded",
            UsageRecorded => "usage.recorded",
            ArtifactCreated => "artifact.created",
            CtxCompacted => "ctx.compacted",
            DocUpserted => "doc.upserted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        use EventKind::*;
        Some(match s {
            "run.started" => RunStarted,
            "turn.started" => TurnStarted,
            "turn.finished" => TurnFinished,
            "msg.assistant" => MsgAssistant,
            "msg.tool_result" => MsgToolResult,
            "msg.synthetic" => MsgSynthetic,
            "tool.call_started" => ToolCallStarted,
            "tool.call_dispatched" => ToolCallDispatched,
            "tool.call_finished" => ToolCallFinished,
            "tool.call_abandoned" => ToolCallAbandoned,
            "approval.requested" => ApprovalRequested,
            "approval.card_sent" => ApprovalCardSent,
            "approval.resolved" => ApprovalResolved,
            "task.enqueued" => TaskEnqueued,
            "task.leased" => TaskLeased,
            "task.finished" => TaskFinished,
            "run.finished" => RunFinished,
            "agent.spawned" => AgentSpawned,
            "agent.concluded" => AgentConcluded,
            "usage.recorded" => UsageRecorded,
            "artifact.created" => ArtifactCreated,
            "ctx.compacted" => CtxCompacted,
            "doc.upserted" => DocUpserted,
            _ => return None,
        })
    }
}
