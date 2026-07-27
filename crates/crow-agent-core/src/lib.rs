//! Security-sensitive local execution primitives shared by the daemon and Tauri shell.

pub mod backtest;
pub mod crypto;
pub mod gateway;
pub mod journal;
pub mod policy;
pub mod scoring;

pub use backtest::{BacktestEngine, BacktestError, BacktestResult, CandleV1};
pub use crypto::{BundleCiphertext, DeviceEncryptionKey, WrappedBundleKey};
pub use gateway::{GatewayClient, GatewayError, InferenceRequest, InferenceResponse};
pub use journal::{EncryptedJournal, JournalError};
pub use policy::{
    MarketState, OrderDecision, PolicyContext, PolicyError, PortfolioState, Proposal, Side,
    evaluate_proposal,
};
pub use scoring::{RunScoreInput, ScoredRun, score_runs};
