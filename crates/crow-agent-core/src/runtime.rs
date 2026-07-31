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
pub const MAX_DECISION_SUMMARY_CHARS: usize = 240;

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
    pub strategy_instructions: String,
    pub cycle_context: Value,
    pub prior_tool_results: Vec<ToolResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurn {
    pub tool_calls: Vec<ToolCall>,
    pub proposal: Option<Proposal>,
    /// Concise user-facing explanation derived from observable cycle facts.
    /// This is receipt-bound structured output, never raw chain-of-thought.
    pub decision_summary: String,
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
    #[error("Crow gateway returned an unsuccessful status")]
    GatewayStatus,
    #[error("Crow gateway request timed out")]
    GatewayTimeout,
    #[error("Crow gateway transport failed")]
    GatewayTransport,
    #[error("Crow gateway receipt binding failed")]
    GatewayReceiptBinding,
    #[error("model returned an invalid structured turn")]
    ModelTurnInvalid,
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
    #[error("model returned an invalid decision summary")]
    DecisionSummary,
    #[error("inference receipt does not bind the model turn")]
    ReceiptBinding,
    #[error("canonical serialization failed")]
    Protocol(#[from] ProtocolError),
    #[error("proposal violated local policy")]
    Policy(#[from] PolicyError),
}

impl RuntimeError {
    /// Stable display-safe classification. It never includes model output,
    /// prompt text, strategy instructions, credentials, or upstream details.
    #[must_use]
    pub const fn failure_class(&self) -> &'static str {
        match self {
            Self::Inference => "inference_failed",
            Self::GatewayStatus => "gateway_http_status",
            Self::GatewayTimeout => "gateway_timeout",
            Self::GatewayTransport => "gateway_transport",
            Self::GatewayReceiptBinding => "gateway_receipt_binding",
            Self::ModelTurnInvalid => "invalid_model_turn",
            Self::Tool => "tool_failed",
            Self::ToolUnavailable => "tool_unavailable",
            Self::ToolRoundLimit => "tool_round_limit",
            Self::AmbiguousTurn => "ambiguous_model_turn",
            Self::MissingProposal => "missing_proposal",
            Self::DecisionSummary => "invalid_decision_summary",
            Self::ReceiptBinding => "receipt_binding_failed",
            Self::Protocol(_) => "protocol_validation_failed",
            Self::Policy(_) => "policy_rejected",
        }
    }
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
    pub strategy_instructions: &'a str,
    pub markets: &'a BTreeMap<String, MarketState>,
    pub portfolios: &'a BTreeMap<String, PortfolioState>,
    pub cycle_context: Value,
}

#[derive(Debug, Clone)]
pub struct CycleOutcome {
    /// A model may deliberately abstain after receiving a valid, signed turn.
    /// `None` is a receipt-backed hold, not a policy rejection and never
    /// permits venue execution.
    pub proposal: Option<Proposal>,
    pub order: Option<Result<OrderDecision, PolicyError>>,
    pub decision_summary: String,
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
        if context.strategy_instructions.trim().is_empty()
            || context.strategy_instructions.chars().count() > 8_192
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
                strategy_instructions: context.strategy_instructions.to_owned(),
                cycle_context: context.cycle_context.clone(),
                prior_tool_results: tool_results.clone(),
            };
            let turn = self.inference.infer(&request).await?;
            validate_receipt(&request, &turn)?;
            let decision_summary = validate_decision_summary(
                &turn.output.decision_summary,
                context.strategy_instructions,
            )?;
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
                    proposal: Some(proposal),
                    order: Some(order),
                    decision_summary,
                    receipts,
                    tool_results,
                });
            }
            if round == MAX_TOOL_ROUNDS {
                return Err(RuntimeError::ToolRoundLimit);
            }
            if turn.output.tool_calls.is_empty() {
                return Ok(CycleOutcome {
                    proposal: None,
                    order: None,
                    decision_summary,
                    receipts,
                    tool_results,
                });
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

fn validate_decision_summary(
    summary: &str,
    strategy_instructions: &str,
) -> Result<String, RuntimeError> {
    if summary
        .chars()
        .any(|character| character.is_control() && !character.is_whitespace())
    {
        return Err(RuntimeError::DecisionSummary);
    }
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = summary.to_ascii_lowercase();
    let normalized_strategy = strategy_instructions.trim().to_ascii_lowercase();
    if summary.is_empty()
        || [
            "system prompt",
            "strategy instruction",
            "my instruction",
            "prompt says",
            "hidden reasoning",
            "chain of thought",
        ]
        .iter()
        .any(|private_reference| normalized.contains(private_reference))
        || (normalized.chars().count() >= 24
            && normalized_strategy.chars().count() >= 24
            && (normalized.contains(&normalized_strategy)
                || normalized_strategy.contains(&normalized)))
    {
        return Err(RuntimeError::DecisionSummary);
    }
    Ok(summary
        .chars()
        .take(MAX_DECISION_SUMMARY_CHARS)
        .collect::<String>()
        .trim_end()
        .to_owned())
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
                    decision_summary: "Checking the latest approved candle evidence.".into(),
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
                    decision_summary: "BTC evidence supports a small policy-compliant long entry."
                        .into(),
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

    #[derive(Debug)]
    struct HoldingInference;

    #[async_trait]
    impl InferenceProvider for HoldingInference {
        async fn infer(&self, request: &ModelTurnRequest) -> Result<InferenceTurn, RuntimeError> {
            let output = ModelTurn {
                tool_calls: Vec::new(),
                proposal: None,
                decision_summary:
                    "Momentum is mixed and does not justify a policy-compliant entry.".into(),
            };
            Ok(InferenceTurn {
                receipt: receipt_for(request, &output)?,
                output,
            })
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
                strategy_instructions: "Prefer verified, policy-compliant holds.",
                markets: &markets,
                portfolios: &portfolios,
                cycle_context: json!({"candle_closed_at": "2026-07-01T00:00:00Z"}),
            })
            .await?;
        assert_eq!(outcome.receipts.len(), 2);
        assert_eq!(outcome.tool_results.len(), 1);
        let order = outcome.order.ok_or(RuntimeError::Inference)?;
        assert_eq!(order?.symbol, "BTC");
        Ok(())
    }

    #[tokio::test]
    async fn signed_model_hold_is_not_a_policy_rejection() -> Result<(), RuntimeError> {
        let runtime = AgentRuntime::new(HoldingInference);
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
                manifest: &manifest(),
                run_id: Uuid::from_u128(20),
                cycle_id: Uuid::from_u128(21),
                model_id: ALLOWED_MODELS[0],
                strategy_instructions: "Hold when no compliant action exists.",
                markets: &markets,
                portfolios: &portfolios,
                cycle_context: json!({"candle_closed_at": "2026-07-01T00:00:00Z"}),
            })
            .await?;
        assert!(outcome.proposal.is_none());
        assert!(outcome.order.is_none());
        assert_eq!(
            outcome.decision_summary,
            "Momentum is mixed and does not justify a policy-compliant entry."
        );
        assert_eq!(outcome.receipts.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn decision_summary_rejects_private_or_unsafe_content() {
        #[derive(Debug)]
        struct InvalidSummaryInference;

        #[async_trait]
        impl InferenceProvider for InvalidSummaryInference {
            async fn infer(
                &self,
                request: &ModelTurnRequest,
            ) -> Result<InferenceTurn, RuntimeError> {
                let output = ModelTurn {
                    tool_calls: Vec::new(),
                    proposal: None,
                    decision_summary: "hidden\nreasoning".into(),
                };
                Ok(InferenceTurn {
                    receipt: receipt_for(request, &output)?,
                    output,
                })
            }
        }

        let runtime = AgentRuntime::new(InvalidSummaryInference);
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
        let portfolios = BTreeMap::from([(
            "BTC".into(),
            PortfolioState {
                equity_micro_usdc: 1_000_000_000,
                available_collateral_micro_usdc: 1_000_000_000,
                trading_day_start_equity_micro_usdc: 1_000_000_000,
                peak_equity_micro_usdc: 1_000_000_000,
                symbol_position_micro_usdc: 0,
                orders_today: 0,
            },
        )]);
        let result = runtime
            .execute_cycle(&CycleContext {
                manifest: &manifest(),
                run_id: Uuid::from_u128(30),
                cycle_id: Uuid::from_u128(31),
                model_id: ALLOWED_MODELS[0],
                strategy_instructions: "Hold when evidence is weak.",
                markets: &markets,
                portfolios: &portfolios,
                cycle_context: json!({"candle_closed_at": "2026-07-01T00:00:00Z"}),
            })
            .await;
        assert!(matches!(result, Err(RuntimeError::DecisionSummary)));
        assert!(matches!(
            validate_decision_summary(
                "My strategy instructions require this trade.",
                "Only buy when every signal agrees."
            ),
            Err(RuntimeError::DecisionSummary)
        ));
        assert!(matches!(
            validate_decision_summary(
                "Hold when the expected edge is weak.",
                "Hold when the expected edge is weak."
            ),
            Err(RuntimeError::DecisionSummary)
        ));
        assert!(matches!(
            validate_decision_summary(
                "BTC evidence is mixed.\u{0000}",
                "Hold when the expected edge is weak."
            ),
            Err(RuntimeError::DecisionSummary)
        ));
    }

    #[test]
    fn decision_summary_normalizes_whitespace_and_length() -> Result<(), RuntimeError> {
        assert_eq!(
            validate_decision_summary(
                "  BTC momentum is mixed,\n\tso no compliant entry is justified.  ",
                "Hold when the expected edge is weak."
            )?,
            "BTC momentum is mixed, so no compliant entry is justified."
        );

        let long_summary = format!("{} tail", "observable market evidence ".repeat(16));
        let bounded =
            validate_decision_summary(&long_summary, "Hold when the expected edge is weak.")?;
        assert_eq!(bounded.chars().count(), MAX_DECISION_SUMMARY_CHARS);
        assert!(!bounded.contains('\n'));
        assert!(
            bounded
                .chars()
                .last()
                .is_some_and(|character| !character.is_whitespace())
        );
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
