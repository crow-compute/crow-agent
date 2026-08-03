use crate::policy::{
    MICRO_USDC_PER_USDC, MarketState, PolicyContext, PolicyError, PortfolioState, Proposal, Side,
    evaluate_proposal,
};
use crow_agent_protocol::{ExecutionAssumptionsV1, RiskRulesV1};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    pub funding_rate_e12: i64,
    pub size_decimals: u8,
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
    pub ending_equity_micro_usdc: i64,
    pub fees_micro_usdc: i64,
    pub funding_micro_usdc: i64,
    pub fills: Vec<SimulatedFill>,
    pub equity_curve: Vec<EquityPoint>,
    pub policy_rejections: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledProposal {
    pub decision_open_time_ms: i64,
    pub proposal: Proposal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub close_time_ms: i64,
    pub equity_micro_usdc: i64,
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
            size_decimals: decision_candle.size_decimals,
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
                starting_capital_micro_usdc: portfolio.equity_micro_usdc,
                quantity_reference_price_micro_usdc: None,
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
            ending_equity_micro_usdc: cash.checked_add(position).ok_or(BacktestError::Overflow)?,
            fees_micro_usdc: fills.iter().try_fold(0_i64, |total, fill| {
                total
                    .checked_add(fill.fee_micro_usdc)
                    .ok_or(BacktestError::Overflow)
            })?,
            funding_micro_usdc: 0,
            fills,
            equity_curve: Vec::new(),
            policy_rejections,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_synchronized_proposals(
        &self,
        candles: &[CandleV1],
        proposals: &[ScheduledProposal],
        starting_cash_micro_usdc: i64,
    ) -> Result<BacktestResult, BacktestError> {
        const SYMBOLS: [&str; 3] = ["BTC", "ETH", "SOL"];
        const DAY_MILLIS: i64 = 86_400_000;
        if starting_cash_micro_usdc <= 0 || candles.len() < 6 || !candles.len().is_multiple_of(3) {
            return Err(BacktestError::CandleOrder);
        }
        let intervals = candles.chunks_exact(3).collect::<Vec<_>>();
        for (index, interval) in intervals.iter().enumerate() {
            let open_time = interval[0].open_time_ms;
            for (symbol_index, candle) in interval.iter().enumerate() {
                if candle.symbol != SYMBOLS[symbol_index]
                    || candle.open_time_ms != open_time
                    || candle.close_time_ms != open_time + 900_000 - 1
                    || candle.size_decimals > 8
                {
                    return Err(BacktestError::CandleOrder);
                }
                if index > 0
                    && candle.open_time_ms
                        != intervals[index - 1][symbol_index].open_time_ms + 900_000
                {
                    return Err(BacktestError::CandleOrder);
                }
            }
        }
        let mut scheduled = BTreeMap::<i64, &Proposal>::new();
        for proposal in proposals {
            if scheduled
                .insert(proposal.decision_open_time_ms, &proposal.proposal)
                .is_some()
            {
                return Err(BacktestError::CandleOrder);
            }
        }

        let mut cash = starting_cash_micro_usdc;
        let mut positions = BTreeMap::<String, i64>::from([
            ("BTC".into(), 0),
            ("ETH".into(), 0),
            ("SOL".into(), 0),
        ]);
        let mut fills = Vec::new();
        let mut equity_curve = Vec::with_capacity(intervals.len());
        let mut fees = 0_i64;
        let mut funding = 0_i64;
        let mut policy_rejections = 0_u32;
        let mut peak_equity = starting_cash_micro_usdc;
        let mut day = intervals[0][0].close_time_ms / DAY_MILLIS;
        let mut day_start_equity = starting_cash_micro_usdc;
        let mut orders_today = 0_u16;

        for (index, interval) in intervals.iter().enumerate() {
            for candle in *interval {
                let quantity = *positions
                    .get(&candle.symbol)
                    .ok_or(BacktestError::CandleOrder)?;
                let notional = position_notional(quantity, candle.close_micro_usdc)?;
                let payment = i64::try_from(
                    i128::from(notional) * i128::from(candle.funding_rate_e12) / 1_000_000_000_000,
                )
                .map_err(|_| BacktestError::Overflow)?;
                cash = cash.checked_sub(payment).ok_or(BacktestError::Overflow)?;
                funding = funding
                    .checked_add(payment)
                    .ok_or(BacktestError::Overflow)?;
            }
            let mut equity = portfolio_equity(cash, &positions, interval)?;
            let current_day = interval[0].close_time_ms / DAY_MILLIS;
            if current_day != day {
                day = current_day;
                day_start_equity = equity;
                orders_today = 0;
            }
            peak_equity = peak_equity.max(equity);

            if let Some(proposal) = scheduled.remove(&interval[0].open_time_ms) {
                if index + 1 >= intervals.len() {
                    return Err(BacktestError::CandleOrder);
                }
                let symbol_index = SYMBOLS
                    .iter()
                    .position(|symbol| *symbol == proposal.symbol)
                    .ok_or(BacktestError::CandleOrder)?;
                let decision = &interval[symbol_index];
                let next = &intervals[index + 1][symbol_index];
                let quantity = *positions
                    .get(&proposal.symbol)
                    .ok_or(BacktestError::CandleOrder)?;
                let position = position_notional(quantity, decision.close_micro_usdc)?;
                let portfolio = PortfolioState {
                    equity_micro_usdc: equity,
                    available_collateral_micro_usdc: cash,
                    trading_day_start_equity_micro_usdc: day_start_equity,
                    peak_equity_micro_usdc: peak_equity,
                    symbol_position_micro_usdc: position,
                    orders_today,
                };
                match self.execute_next_open(decision, next, proposal, &portfolio) {
                    Ok(Some(fill)) => {
                        let notional = position_notional(fill.quantity_e8, fill.price_micro_usdc)?;
                        let held = positions
                            .get_mut(&fill.symbol)
                            .ok_or(BacktestError::CandleOrder)?;
                        match fill.side {
                            Side::Buy => {
                                cash = cash
                                    .checked_sub(notional)
                                    .and_then(|value| value.checked_sub(fill.fee_micro_usdc))
                                    .ok_or(BacktestError::Overflow)?;
                                *held = held
                                    .checked_add(fill.quantity_e8)
                                    .ok_or(BacktestError::Overflow)?;
                            }
                            Side::Sell => {
                                if fill.quantity_e8 > *held {
                                    return Err(BacktestError::Policy(PolicyError::ReduceOnly));
                                }
                                cash = cash
                                    .checked_add(notional)
                                    .and_then(|value| value.checked_sub(fill.fee_micro_usdc))
                                    .ok_or(BacktestError::Overflow)?;
                                *held -= fill.quantity_e8;
                            }
                        }
                        fees = fees
                            .checked_add(fill.fee_micro_usdc)
                            .ok_or(BacktestError::Overflow)?;
                        orders_today = orders_today.saturating_add(1);
                        fills.push(fill);
                    }
                    Ok(None) => {}
                    Err(BacktestError::Policy(_)) => {
                        policy_rejections = policy_rejections.saturating_add(1);
                    }
                    Err(error) => return Err(error),
                }
                equity = portfolio_equity(cash, &positions, interval)?;
                peak_equity = peak_equity.max(equity);
            }
            equity_curve.push(EquityPoint {
                close_time_ms: interval[0].close_time_ms,
                equity_micro_usdc: equity,
            });
        }
        if !scheduled.is_empty() {
            return Err(BacktestError::CandleOrder);
        }
        let ending_equity = portfolio_equity(
            cash,
            &positions,
            intervals.last().ok_or(BacktestError::CandleOrder)?,
        )?;
        Ok(BacktestResult {
            ending_cash_micro_usdc: cash,
            ending_equity_micro_usdc: ending_equity,
            fees_micro_usdc: fees,
            funding_micro_usdc: funding,
            fills,
            equity_curve,
            policy_rejections,
        })
    }
}

fn position_notional(quantity_e8: i64, price_micro_usdc: i64) -> Result<i64, BacktestError> {
    i64::try_from(i128::from(quantity_e8) * i128::from(price_micro_usdc) / 100_000_000)
        .map_err(|_| BacktestError::Overflow)
}

fn portfolio_equity(
    cash: i64,
    positions: &BTreeMap<String, i64>,
    candles: &[CandleV1],
) -> Result<i64, BacktestError> {
    candles.iter().try_fold(cash, |equity, candle| {
        let quantity = *positions
            .get(&candle.symbol)
            .ok_or(BacktestError::CandleOrder)?;
        equity
            .checked_add(position_notional(quantity, candle.close_micro_usdc)?)
            .ok_or(BacktestError::Overflow)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crow_agent_protocol::{canonical_json, sha256};
    use std::error::Error;

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
            funding_rate_e12: 0,
            size_decimals: 5,
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

    #[test]
    fn synchronized_backtest_charges_fees_and_funding_deterministically()
    -> Result<(), BacktestError> {
        let engine = BacktestEngine::new(
            RiskRulesV1::default(),
            ExecutionAssumptionsV1 {
                half_spread_bps: 0,
                slippage_bps: 0,
                taker_fee_bps: 5,
            },
        );
        let mut candles = Vec::new();
        for interval in 0..3_i64 {
            for (symbol, price, decimals) in [
                ("BTC", 60_000_000_000, 5),
                ("ETH", 3_000_000_000, 4),
                ("SOL", 150_000_000, 2),
            ] {
                candles.push(CandleV1 {
                    symbol: symbol.into(),
                    open_time_ms: interval * 900_000,
                    close_time_ms: interval * 900_000 + 899_999,
                    open_micro_usdc: price,
                    high_micro_usdc: price,
                    low_micro_usdc: price,
                    close_micro_usdc: price,
                    volume_e8: 100,
                    funding_rate_e12: 10_000_000,
                    size_decimals: decimals,
                });
            }
        }
        let result = engine.run_synchronized_proposals(
            &candles,
            &[ScheduledProposal {
                decision_open_time_ms: 0,
                proposal: Proposal {
                    symbol: "BTC".into(),
                    side: Side::Buy,
                    notional_bps: 100,
                    limit_price_micro_usdc: 60_000_000_000,
                    reduce_only: false,
                },
            }],
            1_000_000_000,
        )?;
        assert_eq!(result.fills.len(), 1);
        assert!(result.fees_micro_usdc > 0);
        assert!(result.funding_micro_usdc > 0);
        assert_eq!(result.equity_curve.len(), 3);
        assert!(result.ending_equity_micro_usdc < 1_000_000_000);
        Ok(())
    }

    #[test]
    fn synchronized_backtest_matches_cross_platform_golden_digest() -> Result<(), Box<dyn Error>> {
        let engine = BacktestEngine::new(
            RiskRulesV1::default(),
            ExecutionAssumptionsV1 {
                half_spread_bps: 2,
                slippage_bps: 3,
                taker_fee_bps: 5,
            },
        );
        let mut candles = Vec::new();
        for interval in 0..6_i64 {
            for (symbol, price, decimals) in [
                ("BTC", 60_000_000_000 + interval * 100_000_000, 5),
                ("ETH", 3_000_000_000 + interval * 10_000_000, 4),
                ("SOL", 150_000_000 + interval * 1_000_000, 2),
            ] {
                candles.push(CandleV1 {
                    symbol: symbol.into(),
                    open_time_ms: interval * 900_000,
                    close_time_ms: interval * 900_000 + 899_999,
                    open_micro_usdc: price,
                    high_micro_usdc: price + 1_000_000,
                    low_micro_usdc: price - 1_000_000,
                    close_micro_usdc: price + 500_000,
                    volume_e8: 100_000_000 + interval,
                    funding_rate_e12: (interval + 1) * 2_000_000,
                    size_decimals: decimals,
                });
            }
        }
        let result = engine.run_synchronized_proposals(
            &candles,
            &[
                ScheduledProposal {
                    decision_open_time_ms: 0,
                    proposal: Proposal {
                        symbol: "BTC".into(),
                        side: Side::Buy,
                        notional_bps: 100,
                        limit_price_micro_usdc: 70_000_000_000,
                        reduce_only: false,
                    },
                },
                ScheduledProposal {
                    decision_open_time_ms: 900_000,
                    proposal: Proposal {
                        symbol: "ETH".into(),
                        side: Side::Buy,
                        notional_bps: 100,
                        limit_price_micro_usdc: 4_000_000_000,
                        reduce_only: false,
                    },
                },
                ScheduledProposal {
                    decision_open_time_ms: 1_800_000,
                    proposal: Proposal {
                        symbol: "SOL".into(),
                        side: Side::Buy,
                        notional_bps: 300,
                        limit_price_micro_usdc: 200_000_000,
                        reduce_only: false,
                    },
                },
                ScheduledProposal {
                    decision_open_time_ms: 2_700_000,
                    proposal: Proposal {
                        symbol: "BTC".into(),
                        side: Side::Sell,
                        notional_bps: 50,
                        limit_price_micro_usdc: 1,
                        reduce_only: true,
                    },
                },
            ],
            1_000_000_000,
        )?;
        let digest = hex::encode(sha256(&canonical_json(&result)?));
        assert_eq!(
            digest, "67580b3c870a429b5aee61a01007809ac3862c2c5484d6ee36f9ec14e28cd77d",
            "fixed-point replay changed; update only after reviewing the arena semantics"
        );
        Ok(())
    }
}
