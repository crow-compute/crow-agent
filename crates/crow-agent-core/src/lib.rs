//! Security-sensitive local execution primitives shared by the daemon and Tauri shell.

pub mod backtest;
pub mod companion;
pub mod crypto;
pub mod dataset;
pub mod device_auth;
pub mod gateway;
pub mod hyperliquid;
pub mod journal;
pub mod policy;
pub mod runtime;
pub mod scoring;

pub use backtest::{
    BacktestEngine, BacktestError, BacktestResult, CandleV1, EquityPoint, ScheduledProposal,
};
pub use companion::{
    CompanionActionV1, CompanionIpcError, CompanionRequestV1, CompanionResponseV1,
    MAX_COMPANION_MESSAGE_BYTES,
};
pub use crypto::{BundleCiphertext, DeviceEncryptionKey, WrappedBundleKey};
pub use dataset::{DatasetError, DatasetPackage, read_verified_dataset, write_signed_dataset};
pub use device_auth::{
    DeviceAuthorizationClient, DeviceAuthorizationError, DeviceAuthorizationSession, DeviceTokens,
};
pub use gateway::{GatewayClient, GatewayError, InferenceRequest, InferenceResponse};
pub use hyperliquid::{
    BookLevel, BookSnapshot, CoreAsset, HyperliquidBookStream, HyperliquidError, HyperliquidVenue,
};
pub use journal::{EncryptedJournal, JournalError};
pub use policy::{
    MarketState, OrderDecision, PolicyContext, PolicyError, PortfolioState, Proposal, Side,
    evaluate_proposal,
};
pub use runtime::{
    AgentRuntime, AllowedTool, CycleContext, CycleOutcome, InferenceProvider, InferenceTurn,
    LocalTool, ModelTurn, ModelTurnRequest, RuntimeError, ToolCall, ToolResult,
};
pub use scoring::{PerformanceMetrics, RunScoreInput, ScoredRun, performance_metrics, score_runs};
