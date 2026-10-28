//! 内核：端口与状态机。不得依赖任何 IO 实现（reqwest/rusqlite/teloxide/rmcp/image）。

pub mod actor;
pub mod agent;
pub mod budget;
pub mod channel;
pub mod context;
pub mod driver;
pub mod effect;
pub mod env;
pub mod error;
pub mod event;
pub mod ids;
pub mod lanes;
pub mod leak;
pub mod identity;
pub mod policy;
pub mod record;
pub mod retriever;
pub mod schedule;
pub mod render;
pub mod store;
pub mod tool;
pub mod tool_registry;
pub mod runner;
pub mod view;

pub use driver::{
    ImagePart,EventSink, FinishReason, Step, StreamEvent, Turn, TurnDriver, TurnSpec, UsageSnapshot};
pub use channel::{Attachment, Channel, ChannelError, Inbound, InboundKind};
pub use actor::{ActorConfig, ConversationActor};
pub use budget::BudgetLimits;
pub use agent::{
    clip_summary, AgentError, AgentLimits, AgentProfile, AgentSpec, AgentSpawner, AgentStatus,
    Conclusion, RoleSource, RoleSpec,
};
pub use effect::EffectClass;
pub use env::{ExecOpts, ExecOutput, ExecutionEnv};
pub use error::{EngineError, EnvError, ErrorCode, StoreError, ToolError};
pub use event::{Event, EventKind, NewEvent};
pub use ids::*;
pub use identity::{Binding, Directory, User};
pub use policy::{ApprovalPolicy, Domain, Verdict};
pub use lanes::{Lane, LaneCancelled, LaneGuard, LaneLimits, Lanes};
pub use schedule::Schedule;
pub use record::*;
pub use retriever::{Hit, Query, Retriever, SkillSource};
pub use store::{ReadTx, Store, Tx};
pub use tool::{DocReader, NullDocReader, NullOutbox, Outbox, Tool, ToolCallRequest, ToolCtx, ToolOutput, ToolSpec, Workspace};
pub use tool_registry::ToolRegistry;
pub use runner::{RunOutcome, RunRequest, Runner, RunnerError};
pub use view::{Block, Button, Caps, Choice, EditOutcome, Prompt, RenderAction, RenderEvent, Renderer, RunStatus, ToolState, View};
pub use render::{ActionOutcome, TurnRenderer, apply_action};
