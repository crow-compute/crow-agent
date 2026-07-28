use crate::{
    MarketState, OrderDecision, PolicyContext, PolicyError, PortfolioState, Proposal,
    evaluate_proposal,
};
use async_trait::async_trait;
use crow_agent_protocol::{
    ArenaInferenceReceiptV1, ArenaManifestV1, ProtocolError, canonical_json, sha256,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;

const MAX_TOOL_ROUNDS: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowedTool {
    MarketSnapshot,
    PortfolioSnapshot,
    RecentCandles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub tool: AllowedTool,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool: AllowedTool,
    pub output: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub arena_id: Uuid,
    pub run_id: Uuid,
    pub cycle_id: Uuid,
    pub model_id: String,
    pub cycle_context: Value,
    pub prior_tool_results: Vec<ToolResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurn {
    pub tool_calls: Vec<ToolCall>,
    pub proposal: Option<Proposal>,
}

#[derive(Debug, Clone)]
pub struct InferenceTurn {
    pub output: ModelTurn,
    pub receipt: ArenaInferenceReceiptV1,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("model inference failed")]
    Inference,
    #[error("tool execution failed")]
    Tool,
    #[error("model requested an unavailable tool")]
    ToolUnavailable,
    #[error("model exceeded the bounded tool loop")]
    ToolRoundLimit,
    #[error("model returned both tool calls and an order proposal")]
    AmbiguousTurn,
    #[error("model completed without an order proposal")]
    MissingProposal,
    #[error("inference receipt does not bind the model turn")]
    ReceiptBinding,
    #[error("canonical serialization failed")]
    Protocol(#[from] ProtocolError),
    #[error("proposal violated local policy")]
    Policy(#[from] PolicyError),
}

#[async_trait]
pub trait InferenceProvider: Send + Sync {
    async fn infer(&self, request: &ModelTurnRequest) -> Result<InferenceTurn, RuntimeError>;
}

#[async_trait]
pub trait LocalTool: Send + Sync {
    fn kind(&self) -> AllowedTool;

    async fn execute(&self, arguments: &Value) -> Result<Value, RuntimeError>;
}

#[derive(Debug, Clone)]
pub struct CycleContext<'a> {
    pub manifest: &'a ArenaManifestV1,
    pub run_id: Uuid,
    pub cycle_id: Uuid,
    pub model_id: &'a str,
    pub markets: &'a BTreeMap<String, MarketState>,
    pub portfolios: &'a BTreeMap<String, PortfolioState>,
    pub cycle_context: Value,
}

#[derive(Debug, Clone)]
pub struct CycleOutcome {
    pub proposal: Proposal,
    pub order: Result<OrderDecision, PolicyError>,
    pub receipts: Vec<ArenaInferenceReceiptV1>,
    pub tool_results: Vec<ToolResult>,
}

pub struct AgentRuntime<I> {
    inference: I,
    tools: BTreeMap<AllowedTool, Box<dyn LocalTool>>,
}

impl<I> std::fmt::Debug for AgentRuntime<I>
where
    I: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRuntime")
            .field("inference", &self.inference)
            .field("tools", &self.tools.keys())
            .finish()
    }
}

impl<I> AgentRuntime<I>
where
    I: InferenceProvider,
{
    #[must_use]
    pub fn new(inference: I) -> Self {
        Self {
            inference,
            tools: BTreeMap::new(),
        }
    }

    pub fn register_tool(&mut self, tool: Box<dyn LocalTool>) -> Result<(), RuntimeError> {
        let kind = tool.kind();
        if self.tools.insert(kind, tool).is_some() {
            return Err(RuntimeError::ToolUnavailable);
        }
        Ok(())
    }

    pub async fn execute_cycle(
        &self,
        context: &CycleContext<'_>,
    ) -> Result<CycleOutcome, RuntimeError> {
        context.manifest.validate()?;
        if !context
            .manifest
            .eligible_models
            .iter()
            .any(|model| model == context.model_id)
        {
            return Err(RuntimeError::ReceiptBinding);
        }

        let mut tool_results = Vec::new();
        let mut receipts = Vec::new();
        for round in 0..=MAX_TOOL_ROUNDS {
            let request = ModelTurnRequest {
                arena_id: context.manifest.arena_id,
                run_id: context.run_id,
                cycle_id: context.cycle_id,
                model_id: context.model_id.to_owned(),
                cycle_context: context.cycle_context.clone(),
                prior_tool_results: tool_results.clone(),
            };
            let turn = self.inference.infer(&request).await?;
            validate_receipt(&request, &turn)?;
            receipts.push(turn.receipt);

            if !turn.output.tool_calls.is_empty() && turn.output.proposal.is_some() {
                return Err(RuntimeError::AmbiguousTurn);
            }
            if let Some(proposal) = turn.output.proposal {
                let market = context
                    .markets
                    .get(&proposal.symbol)
                    .ok_or(RuntimeError::ToolUnavailable)?;
                let portfolio = context
                    .portfolios
                    .get(&proposal.symbol)
                    .ok_or(RuntimeError::ToolUnavailable)?;
                let order = evaluate_proposal(
                    &proposal,
                    &PolicyContext {
                        rules: &context.manifest.risk_rules,
                        market,
                        portfolio,
                    },
                );
                return Ok(CycleOutcome {
                    proposal,
                    order,
                    receipts,
                    tool_results,
                });
            }
            if round == MAX_TOOL_ROUNDS {
                return Err(RuntimeError::ToolRoundLimit);
            }
            if turn.output.tool_calls.is_empty() {
                return Err(RuntimeError::MissingProposal);
            }
            for call in turn.output.tool_calls {
                let tool = self
                    .tools
                    .get(&call.tool)
                    .ok_or(RuntimeError::ToolUnavailable)?;
                let output = tool.execute(&call.arguments).await?;
                tool_results.push(ToolResult {
                    call_id: call.call_id,
                    tool: call.tool,
                    output,
                });
            }
        }
        Err(RuntimeError::ToolRoundLimit)
    }
}

fn validate_receipt(request: &ModelTurnRequest, turn: &InferenceTurn) -> Result<(), RuntimeError> {
    let expected_output = hex::encode(sha256(&canonical_json(&turn.output)?));
    let expected_tools = hex::encode(sha256(&canonical_json(&turn.output.tool_calls)?));
    let receipt = &turn.receipt;
    if receipt.arena_id != request.arena_id
        || receipt.run_id != request.run_id
        || receipt.cycle_id != request.cycle_id
        || receipt.model_id != request.model_id
        || receipt.output_sha256 != expected_output
        || receipt.tool_calls_sha256 != expected_tools
    {
        return Err(RuntimeError::ReceiptBinding);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Side;
    use crow_agent_protocol::{
        ALLOWED_MODELS, ALLOWED_SYMBOLS, ArenaMode, ExecutionAssumptionsV1, HARNESS_PROTOCOL_V1,
        PenaltyRulesV1, RiskRulesV1, ScoringWeightsV1, TicketConfigV1,
    };
    use serde_json::json;
    use std::sync::Mutex;
    use time::macros::datetime;

    #[derive(Debug)]
    struct ScriptedInference {
        calls: Mutex<u8>,
    }

    #[async_trait]
    impl InferenceProvider for ScriptedInference {
        async fn infer(&self, request: &ModelTurnRequest) -> Result<InferenceTurn, RuntimeError> {
            let mut calls = self.calls.lock().map_err(|_| RuntimeError::Inference)?;
            let output = if *calls == 0 {
                *calls += 1;
                ModelTurn {
                    tool_calls: vec![ToolCall {
                        call_id: "candles-1".into(),
                        tool: AllowedTool::RecentCandles,
                        arguments: json!({"limit": 32}),
                    }],
                    proposal: None,
                }
            } else {
                ModelTurn {
                    tool_calls: Vec::new(),
                    proposal: Some(Proposal {
                        symbol: "BTC".into(),
                        side: Side::Buy,
                        notional_bps: 100,
                        limit_price_micro_usdc: 100_000_000,
                        reduce_only: false,
                    }),
                }
            };
            Ok(InferenceTurn {
                receipt: receipt_for(request, &output)?,
                output,
            })
        }
    }

    #[derive(Debug)]
    struct CandlesTool;

    #[async_trait]
    impl LocalTool for CandlesTool {
        fn kind(&self) -> AllowedTool {
            AllowedTool::RecentCandles
        }

        async fn execute(&self, _arguments: &Value) -> Result<Value, RuntimeError> {
            Ok(json!([{"close": 100_000_000}]))
        }
    }

    #[tokio::test]
    async fn bounded_model_loop_executes_only_after_local_policy() -> Result<(), RuntimeError> {
        let mut runtime = AgentRuntime::new(ScriptedInference {
            calls: Mutex::new(0),
        });
        runtime.register_tool(Box::new(CandlesTool))?;
        let manifest = manifest();
        let markets = BTreeMap::from([(
            "BTC".into(),
            MarketState {
                symbol: "BTC".into(),
                mark_price_micro_usdc: 100_000_000,
                oracle_price_micro_usdc: 100_000_000,
                spread_bps: 4,
                book_age_seconds: 1,
                ask_depth_micro_usdc: 10_000_000,
                bid_depth_micro_usdc: 10_000_000,
                size_decimals: 5,
                delisted: false,
            },
        )]);
        let portfolio = PortfolioState {
            equity_micro_usdc: 1_000_000_000,
            available_collateral_micro_usdc: 1_000_000_000,
            trading_day_start_equity_micro_usdc: 1_000_000_000,
            peak_equity_micro_usdc: 1_000_000_000,
            symbol_position_micro_usdc: 0,
            orders_today: 0,
        };
        let portfolios = BTreeMap::from([("BTC".into(), portfolio)]);
        let outcome = runtime
            .execute_cycle(&CycleContext {
                manifest: &manifest,
                run_id: Uuid::from_u128(10),
                cycle_id: Uuid::from_u128(11),
                model_id: ALLOWED_MODELS[0],
                markets: &markets,
                portfolios: &portfolios,
                cycle_context: json!({"candle_closed_at": "2026-07-01T00:00:00Z"}),
            })
            .await?;
        assert_eq!(outcome.receipts.len(), 2);
        assert_eq!(outcome.tool_results.len(), 1);
        assert_eq!(outcome.order?.symbol, "BTC");
        Ok(())
    }

    fn receipt_for(
        request: &ModelTurnRequest,
        output: &ModelTurn,
    ) -> Result<ArenaInferenceReceiptV1, RuntimeError> {
        Ok(ArenaInferenceReceiptV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            receipt_id: Uuid::new_v4(),
            arena_id: request.arena_id,
            run_id: request.run_id,
            cycle_id: request.cycle_id,
            model_id: request.model_id.clone(),
            model_revision: "test".into(),
            runtime_digest: "a".repeat(64),
            input_sha256: hex::encode(sha256(&canonical_json(request)?)),
            output_sha256: hex::encode(sha256(&canonical_json(output)?)),
            tool_calls_sha256: hex::encode(sha256(&canonical_json(&output.tool_calls)?)),
            input_tokens: 1,
            output_tokens: 1,
            amount_microcredits: 1,
            gateway_public_key: "test".into(),
            gateway_signature: "test".into(),
            finalized_at: datetime!(2026-07-01 00:00 UTC),
        })
    }

    fn manifest() -> ArenaManifestV1 {
        ArenaManifestV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            arena_id: Uuid::from_u128(9),
            manifest_version: 1,
            mode: ArenaMode::HyperliquidTestnet,
            starts_at: datetime!(2026-07-01 00:00 UTC),
            ends_at: datetime!(2026-07-02 00:00 UTC),
            decision_interval_seconds: 900,
            symbols: ALLOWED_SYMBOLS.map(str::to_owned).to_vec(),
            eligible_models: vec![ALLOWED_MODELS[0].into()],
            dataset_sha256: None,
            required_client_version: "0.1.0".into(),
            risk_rules: RiskRulesV1::default(),
            execution: ExecutionAssumptionsV1 {
                half_spread_bps: 2,
                slippage_bps: 3,
                taker_fee_bps: 5,
            },
            scoring: ScoringWeightsV1::default(),
            penalties: PenaltyRulesV1::default(),
            ticket: TicketConfigV1::default(),
        }
    }
}
