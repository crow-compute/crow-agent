use crow_agent_protocol::{ALLOWED_SYMBOLS, RiskRulesV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MICRO_USDC_PER_USDC: i64 = 1_000_000;
pub const MIN_ORDER_MICRO_USDC: i64 = 10 * MICRO_USDC_PER_USDC;
const QUANTITY_SCALE: i64 = 100_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub symbol: String,
    pub side: Side,
    pub notional_bps: u16,
    pub limit_price_micro_usdc: i64,
    pub reduce_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketState {
    pub symbol: String,
    pub mark_price_micro_usdc: i64,
    pub oracle_price_micro_usdc: i64,
    pub spread_bps: u16,
    pub book_age_seconds: u16,
    pub ask_depth_micro_usdc: i64,
    pub bid_depth_micro_usdc: i64,
    pub size_decimals: u8,
    pub delisted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioState {
    pub equity_micro_usdc: i64,
    pub available_collateral_micro_usdc: i64,
    pub trading_day_start_equity_micro_usdc: i64,
    pub peak_equity_micro_usdc: i64,
    pub symbol_position_micro_usdc: i64,
    pub orders_today: u16,
}

#[derive(Debug, Clone)]
pub struct PolicyContext<'a> {
    pub rules: &'a RiskRulesV1,
    pub market: &'a MarketState,
    pub portfolio: &'a PortfolioState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderDecision {
    pub symbol: String,
    pub side: Side,
    pub quantity_e8: i64,
    pub limit_price_micro_usdc: i64,
    pub actual_notional_micro_usdc: i64,
    pub effective_notional_bps: u16,
    pub reduce_only: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PolicyError {
    #[error("symbol is outside the BTC/ETH/SOL arena universe")]
    Symbol,
    #[error("market metadata or price is invalid")]
    Market,
    #[error("market is delisted")]
    Delisted,
    #[error("book evidence is stale")]
    StaleBook,
    #[error("spread exceeds arena policy")]
    Spread,
    #[error("mark/oracle divergence exceeds arena policy")]
    OracleGap,
    #[error("long-only policy rejected a non-reducing sell")]
    LongOnly,
    #[error("daily loss limit reached")]
    DailyLoss,
    #[error("drawdown limit reached")]
    Drawdown,
    #[error("daily order limit reached")]
    OrderLimit,
    #[error("normalized order exceeds maximum order size")]
    OrderSize,
    #[error("normalized order exceeds maximum position size")]
    PositionSize,
    #[error("normalized order violates the collateral reserve")]
    CashReserve,
    #[error("displayed book depth cannot support the normalized IOC")]
    Depth,
    #[error("integer arithmetic overflow")]
    Overflow,
}

#[allow(clippy::too_many_lines)]
pub fn evaluate_proposal(
    proposal: &Proposal,
    context: &PolicyContext<'_>,
) -> Result<OrderDecision, PolicyError> {
    let rules = context.rules;
    let market = context.market;
    let portfolio = context.portfolio;
    if !ALLOWED_SYMBOLS.contains(&proposal.symbol.as_str()) || proposal.symbol != market.symbol {
        return Err(PolicyError::Symbol);
    }
    if market.mark_price_micro_usdc <= 0
        || market.oracle_price_micro_usdc <= 0
        || proposal.limit_price_micro_usdc <= 0
        || market.size_decimals > 8
        || portfolio.equity_micro_usdc <= 0
    {
        return Err(PolicyError::Market);
    }
    if market.delisted {
        return Err(PolicyError::Delisted);
    }
    if market.book_age_seconds > rules.book_max_age_seconds {
        return Err(PolicyError::StaleBook);
    }
    if market.spread_bps > rules.max_spread_bps {
        return Err(PolicyError::Spread);
    }
    let oracle_gap = bps_difference(market.mark_price_micro_usdc, market.oracle_price_micro_usdc)?;
    if oracle_gap > i64::from(rules.max_oracle_gap_bps) {
        return Err(PolicyError::OracleGap);
    }
    if proposal.side == Side::Sell && rules.long_only && !proposal.reduce_only {
        return Err(PolicyError::LongOnly);
    }
    if loss_bps(
        portfolio.trading_day_start_equity_micro_usdc,
        portfolio.equity_micro_usdc,
    )? >= i64::from(rules.daily_loss_bps)
    {
        return Err(PolicyError::DailyLoss);
    }
    if loss_bps(
        portfolio.peak_equity_micro_usdc,
        portfolio.equity_micro_usdc,
    )? >= i64::from(rules.drawdown_bps)
    {
        return Err(PolicyError::Drawdown);
    }
    if portfolio.orders_today >= rules.max_orders_day {
        return Err(PolicyError::OrderLimit);
    }

    let requested = bps_amount(
        portfolio.equity_micro_usdc,
        i64::from(proposal.notional_bps),
    )?;
    let normalized = requested.max(MIN_ORDER_MICRO_USDC);
    let raw_quantity = ceil_div(
        i128::from(normalized) * i128::from(QUANTITY_SCALE),
        i128::from(proposal.limit_price_micro_usdc),
    )?;
    let step = 10_i64
        .checked_pow(u32::from(8 - market.size_decimals))
        .ok_or(PolicyError::Overflow)?;
    let quantity_e8 = round_up(
        i64::try_from(raw_quantity).map_err(|_| PolicyError::Overflow)?,
        step,
    )?;
    let actual_notional = i64::try_from(
        i128::from(quantity_e8) * i128::from(proposal.limit_price_micro_usdc)
            / i128::from(QUANTITY_SCALE),
    )
    .map_err(|_| PolicyError::Overflow)?;
    if actual_notional > bps_amount(portfolio.equity_micro_usdc, i64::from(rules.max_order_bps))? {
        return Err(PolicyError::OrderSize);
    }
    if !proposal.reduce_only
        && portfolio
            .symbol_position_micro_usdc
            .checked_add(actual_notional)
            .ok_or(PolicyError::Overflow)?
            > bps_amount(
                portfolio.equity_micro_usdc,
                i64::from(rules.max_position_bps),
            )?
    {
        return Err(PolicyError::PositionSize);
    }
    let reserve = bps_amount(
        portfolio.equity_micro_usdc,
        i64::from(rules.cash_reserve_bps),
    )?;
    if !proposal.reduce_only
        && portfolio
            .available_collateral_micro_usdc
            .checked_sub(actual_notional)
            .ok_or(PolicyError::Overflow)?
            < reserve
    {
        return Err(PolicyError::CashReserve);
    }
    let available_depth = match proposal.side {
        Side::Buy => market.ask_depth_micro_usdc,
        Side::Sell => market.bid_depth_micro_usdc,
    };
    if actual_notional > available_depth {
        return Err(PolicyError::Depth);
    }
    let effective_bps = ceil_div(
        i128::from(actual_notional) * 10_000,
        i128::from(portfolio.equity_micro_usdc),
    )?;
    Ok(OrderDecision {
        symbol: proposal.symbol.clone(),
        side: proposal.side,
        quantity_e8,
        limit_price_micro_usdc: proposal.limit_price_micro_usdc,
        actual_notional_micro_usdc: actual_notional,
        effective_notional_bps: u16::try_from(effective_bps).map_err(|_| PolicyError::Overflow)?,
        reduce_only: proposal.reduce_only,
    })
}

fn bps_amount(value: i64, bps: i64) -> Result<i64, PolicyError> {
    i64::try_from(i128::from(value) * i128::from(bps) / 10_000).map_err(|_| PolicyError::Overflow)
}

fn bps_difference(left: i64, right: i64) -> Result<i64, PolicyError> {
    let difference = i128::from(left).abs_diff(i128::from(right));
    i64::try_from(difference * 10_000 / i128::from(right).unsigned_abs())
        .map_err(|_| PolicyError::Overflow)
}

fn loss_bps(high: i64, current: i64) -> Result<i64, PolicyError> {
    if high <= 0 || current >= high {
        return Ok(0);
    }
    bps_difference(high, current)
}

fn ceil_div(numerator: i128, denominator: i128) -> Result<i128, PolicyError> {
    if numerator < 0 || denominator <= 0 {
        return Err(PolicyError::Overflow);
    }
    numerator
        .checked_add(denominator - 1)
        .map(|value| value / denominator)
        .ok_or(PolicyError::Overflow)
}

fn round_up(value: i64, step: i64) -> Result<i64, PolicyError> {
    let steps = ceil_div(i128::from(value), i128::from(step))?;
    i64::try_from(steps * i128::from(step)).map_err(|_| PolicyError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(
        rules: &'a RiskRulesV1,
        market: &'a MarketState,
        portfolio: &'a PortfolioState,
    ) -> PolicyContext<'a> {
        PolicyContext {
            rules,
            market,
            portfolio,
        }
    }

    #[test]
    fn normalizes_small_order_to_venue_minimum() -> Result<(), PolicyError> {
        let rules = RiskRulesV1::default();
        let market = MarketState {
            symbol: "ETH".into(),
            mark_price_micro_usdc: 2_000_000_000,
            oracle_price_micro_usdc: 2_000_000_000,
            spread_bps: 2,
            book_age_seconds: 1,
            ask_depth_micro_usdc: 100_000_000,
            bid_depth_micro_usdc: 100_000_000,
            size_decimals: 4,
            delisted: false,
        };
        let portfolio = PortfolioState {
            equity_micro_usdc: 999_000_000,
            available_collateral_micro_usdc: 999_000_000,
            trading_day_start_equity_micro_usdc: 999_000_000,
            peak_equity_micro_usdc: 999_000_000,
            symbol_position_micro_usdc: 0,
            orders_today: 0,
        };
        let order = evaluate_proposal(
            &Proposal {
                symbol: "ETH".into(),
                side: Side::Buy,
                notional_bps: 100,
                limit_price_micro_usdc: 2_000_000_000,
                reduce_only: false,
            },
            &context(&rules, &market, &portfolio),
        )?;
        assert_eq!(order.quantity_e8, 500_000);
        assert_eq!(order.actual_notional_micro_usdc, 10_000_000);
        Ok(())
    }

    #[test]
    fn rejects_non_reducing_sell() {
        let rules = RiskRulesV1::default();
        let market = MarketState {
            symbol: "BTC".into(),
            mark_price_micro_usdc: 60_000_000_000,
            oracle_price_micro_usdc: 60_000_000_000,
            spread_bps: 1,
            book_age_seconds: 1,
            ask_depth_micro_usdc: 100_000_000,
            bid_depth_micro_usdc: 100_000_000,
            size_decimals: 5,
            delisted: false,
        };
        let portfolio = PortfolioState {
            equity_micro_usdc: 1_000_000_000,
            available_collateral_micro_usdc: 1_000_000_000,
            trading_day_start_equity_micro_usdc: 1_000_000_000,
            peak_equity_micro_usdc: 1_000_000_000,
            symbol_position_micro_usdc: 0,
            orders_today: 0,
        };
        let result = evaluate_proposal(
            &Proposal {
                symbol: "BTC".into(),
                side: Side::Sell,
                notional_bps: 100,
                limit_price_micro_usdc: 60_000_000_000,
                reduce_only: false,
            },
            &context(&rules, &market, &portfolio),
        );
        assert_eq!(result, Err(PolicyError::LongOnly));
    }
}
