use crate::policy::{
    MICRO_USDC_PER_USDC, MarketState, PolicyContext, PolicyError, PortfolioState, Proposal, Side,
    evaluate_proposal,
};
use crow_agent_protocol::{ExecutionAssumptionsV1, RiskRulesV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleV1 {
    pub symbol: String,
    pub open_time_ms: i64,
    pub close_time_ms: i64,
    pub open_micro_usdc: i64,
    pub high_micro_usdc: i64,
    pub low_micro_usdc: i64,
    pub close_micro_usdc: i64,
    pub volume_e8: i64,
    pub funding_micros_per_usdc: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedFill {
    pub symbol: String,
    pub side: Side,
    pub quantity_e8: i64,
    pub price_micro_usdc: i64,
    pub fee_micro_usdc: i64,
    pub candle_open_time_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub ending_cash_micro_usdc: i64,
    pub fills: Vec<SimulatedFill>,
    pub policy_rejections: u32,
}

#[derive(Debug, Error)]
pub enum BacktestError {
    #[error("candles are not strictly ordered or do not form a next-candle execution pair")]
    CandleOrder,
    #[error("fixed-point arithmetic overflow")]
    Overflow,
    #[error("policy rejected proposal: {0}")]
    Policy(#[from] PolicyError),
}

#[derive(Debug, Clone)]
pub struct BacktestEngine {
    rules: RiskRulesV1,
    execution: ExecutionAssumptionsV1,
}

impl BacktestEngine {
    #[must_use]
    pub fn new(rules: RiskRulesV1, execution: ExecutionAssumptionsV1) -> Self {
        Self { rules, execution }
    }

    pub fn execute_next_open(
        &self,
        decision_candle: &CandleV1,
        next_candle: &CandleV1,
        proposal: &Proposal,
        portfolio: &PortfolioState,
    ) -> Result<Option<SimulatedFill>, BacktestError> {
        if decision_candle.symbol != next_candle.symbol
            || decision_candle.symbol != proposal.symbol
            || decision_candle.close_time_ms >= next_candle.close_time_ms
            || next_candle.open_time_ms < decision_candle.close_time_ms
        {
            return Err(BacktestError::CandleOrder);
        }
        let direction = match proposal.side {
            Side::Buy => 1_i128,
            Side::Sell => -1_i128,
        };
        let impact_bps = i128::from(self.execution.half_spread_bps + self.execution.slippage_bps);
        let fill_price = i64::try_from(
            i128::from(next_candle.open_micro_usdc) * (10_000_i128 + direction * impact_bps)
                / 10_000,
        )
        .map_err(|_| BacktestError::Overflow)?;
        let crosses = match proposal.side {
            Side::Buy => proposal.limit_price_micro_usdc >= fill_price,
            Side::Sell => proposal.limit_price_micro_usdc <= fill_price,
        };
        if !crosses {
            return Ok(None);
        }
        let market = MarketState {
            symbol: decision_candle.symbol.clone(),
            mark_price_micro_usdc: decision_candle.close_micro_usdc,
            oracle_price_micro_usdc: decision_candle.close_micro_usdc,
            spread_bps: self.execution.half_spread_bps.saturating_mul(2),
            book_age_seconds: 0,
            ask_depth_micro_usdc: i64::MAX / 4,
            bid_depth_micro_usdc: i64::MAX / 4,
            size_decimals: 8,
            delisted: false,
        };
        let mut executable = proposal.clone();
        executable.limit_price_micro_usdc = fill_price;
        let order = evaluate_proposal(
            &executable,
            &PolicyContext {
                rules: &self.rules,
                market: &market,
                portfolio,
            },
        )?;
        let fee = i64::try_from(
            i128::from(order.actual_notional_micro_usdc) * i128::from(self.execution.taker_fee_bps)
                / 10_000,
        )
        .map_err(|_| BacktestError::Overflow)?;
        Ok(Some(SimulatedFill {
            symbol: proposal.symbol.clone(),
            side: proposal.side,
            quantity_e8: order.quantity_e8,
            price_micro_usdc: fill_price,
            fee_micro_usdc: fee,
            candle_open_time_ms: next_candle.open_time_ms,
        }))
    }

    pub fn run_static_proposals(
        &self,
        candles: &[CandleV1],
        proposals: &[Option<Proposal>],
        starting_cash_micro_usdc: i64,
    ) -> Result<BacktestResult, BacktestError> {
        if candles.len() < 2 || proposals.len() + 1 != candles.len() {
            return Err(BacktestError::CandleOrder);
        }
        let mut cash = starting_cash_micro_usdc;
        let mut fills = Vec::new();
        let mut policy_rejections = 0_u32;
        let mut position = 0_i64;
        for (index, proposal) in proposals.iter().enumerate() {
            let Some(proposal) = proposal else {
                continue;
            };
            let portfolio = PortfolioState {
                equity_micro_usdc: starting_cash_micro_usdc,
                available_collateral_micro_usdc: cash,
                trading_day_start_equity_micro_usdc: starting_cash_micro_usdc,
                peak_equity_micro_usdc: starting_cash_micro_usdc,
                symbol_position_micro_usdc: position,
                orders_today: u16::try_from(fills.len()).unwrap_or(u16::MAX),
            };
            match self.execute_next_open(&candles[index], &candles[index + 1], proposal, &portfolio)
            {
                Ok(Some(fill)) => {
                    let notional = i64::try_from(
                        i128::from(fill.quantity_e8) * i128::from(fill.price_micro_usdc)
                            / 100_000_000,
                    )
                    .map_err(|_| BacktestError::Overflow)?;
                    match fill.side {
                        Side::Buy => {
                            cash = cash
                                .checked_sub(notional)
                                .and_then(|value| value.checked_sub(fill.fee_micro_usdc))
                                .ok_or(BacktestError::Overflow)?;
                            position = position
                                .checked_add(notional)
                                .ok_or(BacktestError::Overflow)?;
                        }
                        Side::Sell => {
                            cash = cash
                                .checked_add(notional)
                                .and_then(|value| value.checked_sub(fill.fee_micro_usdc))
                                .ok_or(BacktestError::Overflow)?;
                            position = position.saturating_sub(notional);
                        }
                    }
                    fills.push(fill);
                }
                Ok(None) => {}
                Err(BacktestError::Policy(_)) => {
                    policy_rejections = policy_rejections.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        }
        let _ = MICRO_USDC_PER_USDC;
        Ok(BacktestResult {
            ending_cash_micro_usdc: cash,
            fills,
            policy_rejections,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_executes_only_on_next_open() -> Result<(), BacktestError> {
        let engine = BacktestEngine::new(
            RiskRulesV1::default(),
            ExecutionAssumptionsV1 {
                half_spread_bps: 2,
                slippage_bps: 3,
                taker_fee_bps: 5,
            },
        );
        let decision = CandleV1 {
            symbol: "BTC".into(),
            open_time_ms: 0,
            close_time_ms: 899_999,
            open_micro_usdc: 60_000_000_000,
            high_micro_usdc: 61_000_000_000,
            low_micro_usdc: 59_000_000_000,
            close_micro_usdc: 60_000_000_000,
            volume_e8: 1,
            funding_micros_per_usdc: 0,
        };
        let next = CandleV1 {
            open_time_ms: 900_000,
            close_time_ms: 1_799_999,
            ..decision.clone()
        };
        let portfolio = PortfolioState {
            equity_micro_usdc: 1_000_000_000,
            available_collateral_micro_usdc: 1_000_000_000,
            trading_day_start_equity_micro_usdc: 1_000_000_000,
            peak_equity_micro_usdc: 1_000_000_000,
            symbol_position_micro_usdc: 0,
            orders_today: 0,
        };
        let fill = engine.execute_next_open(
            &decision,
            &next,
            &Proposal {
                symbol: "BTC".into(),
                side: Side::Buy,
                notional_bps: 100,
                limit_price_micro_usdc: 61_000_000_000,
                reduce_only: false,
            },
            &portfolio,
        )?;
        assert_eq!(fill.map(|value| value.candle_open_time_ms), Some(900_000));
        Ok(())
    }
}
