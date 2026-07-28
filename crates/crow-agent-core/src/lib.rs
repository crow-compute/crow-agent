//! Security-sensitive local execution primitives shared by the daemon and Tauri shell.

pub mod backtest;
pub mod companion;
pub mod control_plane;
pub mod crypto;
pub mod dataset;
pub mod device_auth;
pub mod gateway;
pub mod hyperliquid;
pub mod journal;
pub mod live;
pub mod live_cycle;
pub mod policy;
pub mod runtime;
pub mod scoring;
pub mod tls;

pub use backtest::{
    BacktestEngine, BacktestError, BacktestResult, CandleV1, EquityPoint, ScheduledProposal,
};
pub use companion::{
    CompanionActionV1, CompanionIpcError, CompanionRequestV1, CompanionResponseV1,
    MAX_COMPANION_MESSAGE_BYTES,
};
pub use control_plane::{
    HarnessApiClient, HarnessApiError, HarnessRunV1, StartHarnessRunV1, StartedHarnessRunV1,
};
pub use crypto::{BundleCiphertext, DeviceEncryptionKey, WrappedBundleKey};
pub use dataset::{
    DATASET_MANIFEST_FILE, DatasetError, DatasetPackage, InstrumentV1, read_verified_dataset,
    write_signed_dataset,
};
pub use device_auth::{
    DeviceAuthorizationClient, DeviceAuthorizationError, DeviceAuthorizationSession, DeviceTokens,
};
pub use gateway::{GatewayClient, GatewayError, InferenceRequest, InferenceResponse};
pub use hyperliquid::{
    AccountSnapshot, BookLevel, BookSnapshot, CoreAsset, HyperliquidBookStream, HyperliquidError,
    HyperliquidVenue, MarketSnapshot, PositionSnapshot, VenueSubmission,
};
pub use journal::{EncryptedJournal, JournalError};
pub use live::{DurableRunEventError, DurableRunEventWriter, RunEventSink};
pub use live_cycle::{
    LiveCycleError, LiveCycleResult, LiveRiskState, LiveVenue, execute_live_cycle,
    load_live_risk_state, reconcile_live_state, store_live_risk_state,
};
pub use policy::{
    MarketState, OrderDecision, PolicyContext, PolicyError, PortfolioState, Proposal, Side,
    evaluate_proposal,
};
pub use runtime::{
    AgentRuntime, AllowedTool, CycleContext, CycleOutcome, InferenceProvider, InferenceTurn,
    LocalTool, ModelTurn, ModelTurnRequest, RuntimeError, ToolCall, ToolResult,
};
pub use scoring::{PerformanceMetrics, RunScoreInput, ScoredRun, performance_metrics, score_runs};
pub use tls::{TlsProviderError, install_tls_crypto_provider};
