use crate::{
    AccountSnapshot, AgentRuntime, AllowedTool, BookSnapshot, DurableRunEventError,
    DurableRunEventWriter, EncryptedJournal, HyperliquidError, HyperliquidVenue, InferenceProvider,
    LocalTool, MarketSnapshot, OrderDecision, PortfolioState, RunEventSink, RuntimeError,
    VenueSubmission,
};
use async_trait::async_trait;
use crow_agent_protocol::{ArenaManifestV1, DeviceIdentity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thiserror::Error;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum LiveCycleError {
    #[error("live venue reconciliation failed")]
    Venue(#[from] HyperliquidError),
    #[error("live model cycle failed")]
    Runtime(#[from] RuntimeError),
    #[error("live run event could not be durably accepted")]
    Event(#[from] DurableRunEventError),
    #[error("live risk state could not be encrypted")]
    Journal(#[from] crate::JournalError),
    #[error("live risk state is invalid")]
    RiskState,
    #[error("live account violates isolated one-times policy")]
    AccountPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRiskState {
    pub trading_day: Date,
    pub trading_day_start_equity_micro_usdc: i64,
    pub peak_equity_micro_usdc: i64,
    pub orders_today: u16,
    pub last_reconciliation_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveCycleResult {
    pub cycle_id: Uuid,
    pub proposal_symbol: String,
    pub policy_allowed: bool,
    pub order_submitted: bool,
    pub client_order_id: Option<String>,
    pub last_event_sha256: String,
}

#[async_trait]
pub trait LiveVenue: Send + Sync {
    async fn account_snapshot(
        &self,
        execution_account: &str,
    ) -> Result<AccountSnapshot, HyperliquidError>;

    async fn market_snapshots(
        &self,
        books: Vec<BookSnapshot>,
        now_ms: u64,
    ) -> Result<BTreeMap<String, MarketSnapshot>, HyperliquidError>;

    async fn recent_candles(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError>;

    async fn submit_ioc(
        &self,
        order: &OrderDecision,
        client_order_id: &str,
    ) -> Result<VenueSubmission, HyperliquidError>;

    async fn fills_since(
        &self,
        execution_account: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError>;

    async fn funding_since(
        &self,
        execution_account: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError>;
}

#[async_trait]
impl LiveVenue for HyperliquidVenue {
    async fn account_snapshot(
        &self,
        execution_account: &str,
    ) -> Result<AccountSnapshot, HyperliquidError> {
        HyperliquidVenue::account_snapshot(self, execution_account).await
    }

    async fn market_snapshots(
        &self,
        books: Vec<BookSnapshot>,
        now_ms: u64,
    ) -> Result<BTreeMap<String, MarketSnapshot>, HyperliquidError> {
        HyperliquidVenue::market_snapshots(self, books, now_ms).await
    }

    async fn recent_candles(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError> {
        HyperliquidVenue::recent_candles(self, start_time_ms, end_time_ms).await
    }

    async fn submit_ioc(
        &self,
        order: &OrderDecision,
        client_order_id: &str,
    ) -> Result<VenueSubmission, HyperliquidError> {
        HyperliquidVenue::submit_ioc(self, order, client_order_id).await
    }

    async fn fills_since(
        &self,
        execution_account: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError> {
        HyperliquidVenue::fills_since(self, execution_account, start_time_ms, end_time_ms).await
    }

    async fn funding_since(
        &self,
        execution_account: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError> {
        HyperliquidVenue::funding_since(self, execution_account, start_time_ms, end_time_ms).await
    }
}

#[derive(Debug, Clone)]
struct SnapshotTool {
    kind: AllowedTool,
    output: Value,
}

#[async_trait]
impl LocalTool for SnapshotTool {
    fn kind(&self) -> AllowedTool {
        self.kind
    }

    async fn execute(&self, _arguments: &Value) -> Result<Value, RuntimeError> {
        Ok(self.output.clone())
    }
}

pub fn load_live_risk_state(
    journal: &EncryptedJournal,
    run_id: Uuid,
) -> Result<Option<LiveRiskState>, LiveCycleError> {
    let key = format!("live-risk-state-{run_id}");
    journal
        .secret(&key)?
        .map(|value| serde_json::from_slice(&value).map_err(|_| LiveCycleError::RiskState))
        .transpose()
}

pub fn store_live_risk_state(
    journal: &EncryptedJournal,
    run_id: Uuid,
    state: &LiveRiskState,
) -> Result<(), LiveCycleError> {
    let key = format!("live-risk-state-{run_id}");
    let encoded = serde_json::to_vec(state).map_err(|_| LiveCycleError::RiskState)?;
    journal.put_secret(&key, &encoded)?;
    Ok(())
}

fn store_live_risk_state_with_writer<S>(
    writer: &DurableRunEventWriter<'_, S>,
    run_id: Uuid,
    state: &LiveRiskState,
) -> Result<(), LiveCycleError>
where
    S: RunEventSink,
{
    let key = format!("live-risk-state-{run_id}");
    let encoded = serde_json::to_vec(state).map_err(|_| LiveCycleError::RiskState)?;
    writer.put_local_secret(&key, &encoded)?;
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn execute_live_cycle<I, S, V>(
    journal: &mut EncryptedJournal,
    sink: &S,
    identity: &DeviceIdentity,
    manifest: &ArenaManifestV1,
    run_id: Uuid,
    model_id: &str,
    execution_account: &str,
    venue: &V,
    books: Vec<BookSnapshot>,
    inference: I,
    risk: &mut LiveRiskState,
    order_permitted: impl Fn() -> bool,
) -> Result<LiveCycleResult, LiveCycleError>
where
    I: InferenceProvider,
    S: RunEventSink,
    V: LiveVenue,
{
    let cycle_id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let now_ms = unix_milliseconds(now)?;
    let candle_start_ms = now_ms.saturating_sub(32 * 900_000);
    let account = venue.account_snapshot(execution_account).await?;
    validate_account_policy(&account)?;
    update_risk_state(risk, &account, now.date());
    let snapshots = venue.market_snapshots(books, now_ms).await?;
    let markets = snapshots
        .iter()
        .map(|(symbol, snapshot)| (symbol.clone(), snapshot.market.clone()))
        .collect::<BTreeMap<_, _>>();
    let portfolios = markets
        .keys()
        .map(|symbol| {
            (
                symbol.clone(),
                PortfolioState {
                    equity_micro_usdc: account.equity_micro_usdc,
                    available_collateral_micro_usdc: account.withdrawable_micro_usdc,
                    trading_day_start_equity_micro_usdc: risk.trading_day_start_equity_micro_usdc,
                    peak_equity_micro_usdc: risk.peak_equity_micro_usdc,
                    symbol_position_micro_usdc: account
                        .positions
                        .get(symbol)
                        .map_or(0, |position| position.notional_micro_usdc),
                    orders_today: risk.orders_today,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let candles = venue.recent_candles(candle_start_ms, now_ms).await?;

    let market_value = serde_json::to_value(&snapshots).map_err(|_| LiveCycleError::RiskState)?;
    let portfolio_value = serde_json::to_value(&account).map_err(|_| LiveCycleError::RiskState)?;
    let mut runtime = AgentRuntime::new(inference);
    runtime.register_tool(Box::new(SnapshotTool {
        kind: AllowedTool::MarketSnapshot,
        output: market_value.clone(),
    }))?;
    runtime.register_tool(Box::new(SnapshotTool {
        kind: AllowedTool::PortfolioSnapshot,
        output: portfolio_value.clone(),
    }))?;
    runtime.register_tool(Box::new(SnapshotTool {
        kind: AllowedTool::RecentCandles,
        output: candles.clone(),
    }))?;

    let mut writer = DurableRunEventWriter::new(journal, sink, identity, manifest.arena_id, run_id);
    writer
        .append(
            Some(cycle_id),
            "cycle_started",
            json!({"scheduled_at": now, "venue_time_ms": now_ms}),
            &Value::Null,
        )
        .await?;
    writer
        .append(
            Some(cycle_id),
            "book_snapshot",
            market_value.clone(),
            &Value::Null,
        )
        .await?;
    writer
        .append(
            Some(cycle_id),
            "candle_closed",
            candles.clone(),
            &Value::Null,
        )
        .await?;
    writer
        .append(
            Some(cycle_id),
            "portfolio_snapshot",
            json!({
                "equity_micro_usdc": account.equity_micro_usdc,
                "venue_time_ms": account.venue_time_ms,
                "positions": account.positions,
            }),
            &Value::Null,
        )
        .await?;

    let cycle_context = json!({
        "scheduled_at": now,
        "market_snapshot": market_value,
        "portfolio_snapshot": portfolio_value,
        "recent_candles": candles,
        "risk_state": risk,
    });
    let outcome = runtime
        .execute_cycle(&crate::CycleContext {
            manifest,
            run_id,
            cycle_id,
            model_id,
            markets: &markets,
            portfolios: &portfolios,
            cycle_context: cycle_context.clone(),
        })
        .await?;
    for receipt in &outcome.receipts {
        writer
            .append(
                Some(cycle_id),
                "inference_receipt",
                json!({"receipt_id": receipt.receipt_id}),
                &Value::Null,
            )
            .await?;
    }
    writer
        .append(
            Some(cycle_id),
            "proposal",
            serde_json::to_value(&outcome.proposal).map_err(|_| LiveCycleError::RiskState)?,
            &json!({
                "cycle_context": cycle_context,
                "tool_results": outcome.tool_results,
            }),
        )
        .await?;

    let proposal_symbol = outcome.proposal.symbol.clone();
    let order = match outcome.order {
        Ok(order) => order,
        Err(policy_error) => {
            let policy_event = writer
                .append(
                    Some(cycle_id),
                    "policy_outcome",
                    json!({"allowed": false, "reason": policy_error.to_string()}),
                    &Value::Null,
                )
                .await?;
            store_live_risk_state_with_writer(&writer, run_id, risk)?;
            return Ok(LiveCycleResult {
                cycle_id,
                proposal_symbol,
                policy_allowed: false,
                order_submitted: false,
                client_order_id: None,
                last_event_sha256: policy_event.event_sha256,
            });
        }
    };
    if !order_permitted() {
        let blocked_event = writer
            .append(
                Some(cycle_id),
                "policy_outcome",
                json!({"allowed": false, "reason": "execution_gate_closed"}),
                &Value::Null,
            )
            .await?;
        store_live_risk_state_with_writer(&writer, run_id, risk)?;
        return Ok(LiveCycleResult {
            cycle_id,
            proposal_symbol,
            policy_allowed: false,
            order_submitted: false,
            client_order_id: None,
            last_event_sha256: blocked_event.event_sha256,
        });
    }
    let policy_event = writer
        .append(
            Some(cycle_id),
            "policy_outcome",
            json!({"allowed": true, "order": order}),
            &Value::Null,
        )
        .await?;
    let client_order_id = Uuid::new_v4().simple().to_string();
    writer
        .append(
            Some(cycle_id),
            "order_submitted",
            json!({
                "client_order_id": client_order_id,
                "policy_event_sha256": policy_event.event_sha256,
                "order": order,
                "phase": "dispatching",
            }),
            &Value::Null,
        )
        .await?;
    risk.orders_today = risk.orders_today.saturating_add(1);
    store_live_risk_state_with_writer(&writer, run_id, risk)?;
    let submission = venue.submit_ioc(&order, &client_order_id).await?;
    writer
        .append(
            Some(cycle_id),
            "venue_acknowledgement",
            json!({
                "client_order_id": submission.client_order_id,
                "statuses": submission.statuses,
            }),
            &Value::Null,
        )
        .await?;

    let reconcile_at = unix_milliseconds(OffsetDateTime::now_utc())?;
    let fills = venue
        .fills_since(execution_account, risk.last_reconciliation_ms, reconcile_at)
        .await?;
    let funding = venue
        .funding_since(execution_account, risk.last_reconciliation_ms, reconcile_at)
        .await?;
    let reconciled = venue.account_snapshot(execution_account).await?;
    validate_account_policy(&reconciled)?;
    risk.last_reconciliation_ms = reconcile_at;
    update_risk_state(risk, &reconciled, now.date());
    writer
        .append(
            Some(cycle_id),
            "fill",
            json!({"fills": fills}),
            &Value::Null,
        )
        .await?;
    writer
        .append(
            Some(cycle_id),
            "funding",
            json!({"funding": funding}),
            &Value::Null,
        )
        .await?;
    writer
        .append(
            Some(cycle_id),
            "reconciliation",
            json!({"venue_time_ms": reconciled.venue_time_ms, "positions": reconciled.positions}),
            &Value::Null,
        )
        .await?;
    let final_event = writer
        .append(
            Some(cycle_id),
            "portfolio_snapshot",
            json!({
                "equity_micro_usdc": reconciled.equity_micro_usdc,
                "venue_time_ms": reconciled.venue_time_ms,
                "positions": reconciled.positions,
            }),
            &Value::Null,
        )
        .await?;
    store_live_risk_state_with_writer(&writer, run_id, risk)?;
    Ok(LiveCycleResult {
        cycle_id,
        proposal_symbol,
        policy_allowed: true,
        order_submitted: true,
        client_order_id: Some(submission.client_order_id),
        last_event_sha256: final_event.event_sha256,
    })
}

fn update_risk_state(state: &mut LiveRiskState, account: &AccountSnapshot, today: Date) {
    if state.trading_day != today {
        state.trading_day = today;
        state.trading_day_start_equity_micro_usdc = account.equity_micro_usdc;
        state.orders_today = 0;
    }
    state.peak_equity_micro_usdc = state.peak_equity_micro_usdc.max(account.equity_micro_usdc);
}

fn validate_account_policy(account: &AccountSnapshot) -> Result<(), LiveCycleError> {
    if account.equity_micro_usdc <= 0
        || account.positions.values().any(|position| {
            position.quantity_e8 < 0 || !position.isolated || position.leverage != 1
        })
    {
        return Err(LiveCycleError::AccountPolicy);
    }
    Ok(())
}

fn unix_milliseconds(value: OffsetDateTime) -> Result<u64, LiveCycleError> {
    u64::try_from(value.unix_timestamp_nanos() / 1_000_000).map_err(|_| LiveCycleError::RiskState)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PositionSnapshot;
    use crow_agent_protocol::ALLOWED_SYMBOLS;
    use tempfile::tempdir;

    #[test]
    fn risk_state_is_encrypted_and_recovers_without_reset() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempdir()?;
        let path = directory.path().join("journal.db");
        let run_id = Uuid::new_v4();
        let state = LiveRiskState {
            trading_day: OffsetDateTime::now_utc().date(),
            trading_day_start_equity_micro_usdc: 1_000_000,
            peak_equity_micro_usdc: 1_100_000,
            orders_today: 7,
            last_reconciliation_ms: 42,
        };
        let journal = EncryptedJournal::open(&path, [41_u8; 32])?;
        store_live_risk_state(&journal, run_id, &state)?;
        assert_eq!(load_live_risk_state(&journal, run_id)?, Some(state));
        let raw = std::fs::read(path)?;
        assert!(
            !raw.windows(b"1100000".len())
                .any(|value| value == b"1100000")
        );
        Ok(())
    }

    #[test]
    fn account_policy_rejects_cross_leverage_and_shorts() {
        let position = PositionSnapshot {
            symbol: ALLOWED_SYMBOLS[0].into(),
            quantity_e8: 1,
            notional_micro_usdc: 1,
            entry_price_micro_usdc: Some(1),
            unrealized_pnl_micro_usdc: 0,
            isolated: false,
            leverage: 1,
        };
        let account = AccountSnapshot {
            venue_time_ms: 1,
            equity_micro_usdc: 1,
            withdrawable_micro_usdc: 1,
            positions: BTreeMap::from([(ALLOWED_SYMBOLS[0].into(), position)]),
        };
        assert!(validate_account_policy(&account).is_err());
    }
}
