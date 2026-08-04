use crate::{OrderDecision, Side};
use crow_agent_protocol::ALLOWED_SYMBOLS;
use futures_util::StreamExt;
use hypersdk::{
    Address, Decimal,
    hypercore::{
        self, CandleInterval, Cloid, HttpClient, NonceHandler, PerpMarket, PrivateKeySigner,
        WebSocket,
        types::{
            AbstractionMode, BatchCancel, BatchOrder, Cancel, Incoming, OrderGrouping,
            OrderRequest, OrderResponseStatus, OrderTypePlacement, PerpAssetCtx, Subscription,
            TimeInForce, UserBalance,
        },
        ws::Event,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use zeroize::Zeroizing;

const REST_WEIGHT_PER_MINUTE: u16 = 1_000;
const INFO_META_WEIGHT: u16 = 20;
const INFO_L2_WEIGHT: u16 = 2;
const INFO_USER_MODE_WEIGHT: u16 = 20;
const INFO_ACCOUNT_STATE_WEIGHT: u16 = 2;
const INFO_USER_HISTORY_WEIGHT: u16 = 20;
const INFO_CANDLE_WEIGHT: u16 = 20;
const INFO_ACTIVE_ASSET_WEIGHT: u16 = 2;
const MARKET_IOC_SLIPPAGE_BPS: i128 = 500;
const BPS_SCALE: i128 = 10_000;
const QUANTITY_E8_SCALE: i128 = 100_000_000;

#[derive(Debug, Error)]
pub enum HyperliquidError {
    #[error("Hyperliquid SDK request failed")]
    Sdk,
    #[error("API wallet key is invalid")]
    Wallet,
    #[error("required BTC/ETH/SOL perpetual metadata is unavailable")]
    Metadata,
    #[error("order symbol does not match discovered core metadata")]
    Symbol,
    #[error("fixed-point venue value cannot be represented")]
    Numeric,
    #[error("local Hyperliquid request budget is exhausted")]
    RateLimit,
    #[error("Hyperliquid market stream ended")]
    StreamClosed,
    #[error("Hyperliquid order book payload is invalid")]
    Book,
    #[error("Hyperliquid account or market snapshot is invalid")]
    Snapshot,
    #[error("Hyperliquid returned a resting or trigger state for an IOC order")]
    IocInvariant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreAsset {
    pub symbol: String,
    pub asset_index: u32,
    pub size_decimals: u8,
    pub max_leverage: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: String,
    pub size: String,
    pub order_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub symbol: String,
    pub venue_time_ms: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub symbol: String,
    pub quantity_e8: i64,
    pub notional_micro_usdc: i64,
    pub entry_price_micro_usdc: Option<i64>,
    pub unrealized_pnl_micro_usdc: i64,
    pub isolated: bool,
    pub leverage: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub venue_time_ms: u64,
    pub equity_micro_usdc: i64,
    pub withdrawable_micro_usdc: i64,
    pub positions: BTreeMap<String, PositionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSnapshot {
    pub market: crate::MarketState,
    pub book: BookSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueSubmission {
    pub client_order_id: String,
    pub statuses: Value,
}

impl BookSnapshot {
    #[must_use]
    pub fn is_fresh_at(&self, now_ms: u64, maximum_age: Duration) -> bool {
        now_ms >= self.venue_time_ms
            && now_ms - self.venue_time_ms
                <= u64::try_from(maximum_age.as_millis()).unwrap_or(u64::MAX)
    }

    /// Returns the deepest displayed opposing price inside Hyperliquid's
    /// standard 5% market-order slippage ceiling. The returned price comes
    /// directly from the venue book, so it already obeys that asset's tick
    /// rules. An IOC may sweep better resting levels but can never rest.
    pub fn marketable_limit_price_micro_usdc(&self, side: Side) -> Result<i64, HyperliquidError> {
        let levels = self.opposing_levels(side)?;
        let top = decimal_string_to_fixed(&levels[0].price, 6)?;
        if top <= 0 {
            return Err(HyperliquidError::Book);
        }
        let mut limit = top;
        for level in levels {
            let price = decimal_string_to_fixed(&level.price, 6)?;
            if price <= 0 {
                return Err(HyperliquidError::Book);
            }
            if !within_market_slippage(side, top, price) {
                break;
            }
            limit = price;
        }
        Ok(limit)
    }

    /// Returns the current best opposing quote. Quantity normalization uses
    /// this executable price so the IOC remains at or above the venue's
    /// minimum notional even when its protective limit is padded deeper.
    pub fn opposing_top_price_micro_usdc(&self, side: Side) -> Result<i64, HyperliquidError> {
        let levels = self.opposing_levels(side)?;
        let top = decimal_string_to_fixed(&levels[0].price, 6)?;
        if top <= 0 {
            return Err(HyperliquidError::Book);
        }
        Ok(top)
    }

    fn marketable_depth_micro_usdc(&self, side: Side) -> Result<i64, HyperliquidError> {
        let levels = self.opposing_levels(side)?;
        let top = decimal_string_to_fixed(&levels[0].price, 6)?;
        if top <= 0 {
            return Err(HyperliquidError::Book);
        }
        let mut depth = 0_i128;
        for level in levels {
            let price = decimal_string_to_fixed(&level.price, 6)?;
            let level_quantity_e8 = decimal_string_to_fixed(&level.size, 8)?;
            if price <= 0 || level_quantity_e8 <= 0 {
                return Err(HyperliquidError::Book);
            }
            if !within_market_slippage(side, top, price) {
                break;
            }
            depth = depth
                .checked_add(
                    i128::from(price)
                        .checked_mul(i128::from(level_quantity_e8))
                        .ok_or(HyperliquidError::Numeric)?
                        / QUANTITY_E8_SCALE,
                )
                .ok_or(HyperliquidError::Numeric)?;
        }
        i64::try_from(depth).map_err(|_| HyperliquidError::Numeric)
    }

    fn opposing_levels(&self, side: Side) -> Result<&[BookLevel], HyperliquidError> {
        let levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        if levels.is_empty() {
            return Err(HyperliquidError::Book);
        }
        Ok(levels)
    }
}

fn within_market_slippage(side: Side, top: i64, candidate: i64) -> bool {
    let candidate = i128::from(candidate) * BPS_SCALE;
    let top = i128::from(top);
    match side {
        Side::Buy => candidate <= top * (BPS_SCALE + MARKET_IOC_SLIPPAGE_BPS),
        Side::Sell => candidate >= top * (BPS_SCALE - MARKET_IOC_SLIPPAGE_BPS),
    }
}

#[derive(Debug)]
struct RequestBudget {
    state: Mutex<RequestBudgetState>,
}

#[derive(Debug)]
struct RequestBudgetState {
    opened_at: Instant,
    used: u16,
}

impl RequestBudget {
    fn new() -> Self {
        Self {
            state: Mutex::new(RequestBudgetState {
                opened_at: Instant::now(),
                used: 0,
            }),
        }
    }

    fn consume(&self, weight: u16) -> Result<(), HyperliquidError> {
        let mut state = self.state.lock().map_err(|_| HyperliquidError::RateLimit)?;
        if state.opened_at.elapsed() >= Duration::from_mins(1) {
            state.opened_at = Instant::now();
            state.used = 0;
        }
        if state.used.saturating_add(weight) > REST_WEIGHT_PER_MINUTE {
            return Err(HyperliquidError::RateLimit);
        }
        state.used += weight;
        Ok(())
    }
}

pub struct HyperliquidVenue {
    client: HttpClient,
    signer: PrivateKeySigner,
    nonce: NonceHandler,
    assets: BTreeMap<String, CoreAsset>,
    budget: Arc<RequestBudget>,
    network: HyperliquidNetwork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperliquidNetwork {
    Testnet,
    Mainnet,
}

impl std::fmt::Debug for HyperliquidVenue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HyperliquidVenue")
            .field("network", &self.network)
            .field("assets", &self.assets)
            .field("api_wallet", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl HyperliquidVenue {
    pub async fn connect_testnet(
        api_wallet_key: &Zeroizing<[u8; 32]>,
    ) -> Result<Self, HyperliquidError> {
        Self::connect(api_wallet_key, HyperliquidNetwork::Testnet).await
    }

    pub async fn connect_mainnet(
        api_wallet_key: &Zeroizing<[u8; 32]>,
    ) -> Result<Self, HyperliquidError> {
        Self::connect(api_wallet_key, HyperliquidNetwork::Mainnet).await
    }

    async fn connect(
        api_wallet_key: &Zeroizing<[u8; 32]>,
        network: HyperliquidNetwork,
    ) -> Result<Self, HyperliquidError> {
        let encoded_key = Zeroizing::new(format!("0x{}", hex::encode(api_wallet_key.as_ref())));
        let signer = encoded_key
            .parse::<PrivateKeySigner>()
            .map_err(|_| HyperliquidError::Wallet)?;
        let budget = Arc::new(RequestBudget::new());
        budget.consume(INFO_META_WEIGHT)?;
        let client = match network {
            HyperliquidNetwork::Testnet => hypercore::testnet(),
            HyperliquidNetwork::Mainnet => hypercore::mainnet(),
        };
        let markets = client.perps().await.map_err(|_| HyperliquidError::Sdk)?;
        let assets = discover_core_assets(&markets)?;
        Ok(Self {
            client,
            signer,
            nonce: NonceHandler::default(),
            assets,
            budget,
            network,
        })
    }

    #[must_use]
    pub fn assets(&self) -> &BTreeMap<String, CoreAsset> {
        &self.assets
    }

    pub async fn submit_ioc(
        &self,
        order: &OrderDecision,
        client_order_id: &str,
    ) -> Result<VenueSubmission, HyperliquidError> {
        self.budget.consume(1)?;
        let cloid = format!("0x{client_order_id}")
            .parse::<Cloid>()
            .map_err(|_| HyperliquidError::Numeric)?;
        let request = build_ioc_request(order, &self.assets, cloid)?;
        let statuses = self
            .client
            .place(
                &self.signer,
                BatchOrder {
                    orders: vec![request],
                    grouping: OrderGrouping::Na,
                    builder: None,
                },
                self.nonce.next(),
                None,
                None,
            )
            .await
            .map_err(|_| HyperliquidError::Sdk)?;
        for status in &statuses {
            match status {
                OrderResponseStatus::Resting { oid, .. } => {
                    self.cancel_direct(&order.symbol, *oid).await?;
                    return Err(HyperliquidError::IocInvariant);
                }
                OrderResponseStatus::WaitingForTrigger | OrderResponseStatus::WaitingForFill => {
                    return Err(HyperliquidError::IocInvariant);
                }
                _ => {}
            }
        }
        let statuses = statuses
            .iter()
            .map(|status| {
                json!({
                    "accepted": status.is_ok(),
                    "order_id": status.oid().map(|value| value.to_string()),
                    "reason": status.error().map(display_safe_venue_rejection),
                    "state": if status.is_err() {
                        "venue_rejected"
                    } else if status.oid().is_some() {
                        "acknowledged"
                    } else {
                        "accepted"
                    },
                })
            })
            .collect::<Vec<_>>();
        Ok(VenueSubmission {
            client_order_id: client_order_id.to_owned(),
            statuses: Value::Array(statuses),
        })
    }

    pub async fn configure_isolated_leverage(
        &self,
        execution_account: &str,
        leverage: u8,
    ) -> Result<(), HyperliquidError> {
        let leverage = u32::from(leverage);
        if leverage == 0
            || self
                .assets
                .values()
                .any(|asset| leverage > asset.max_leverage)
        {
            return Err(HyperliquidError::Metadata);
        }
        let account = parse_account(execution_account)?;
        for symbol in ALLOWED_SYMBOLS {
            let asset = self.assets.get(symbol).ok_or(HyperliquidError::Metadata)?;
            self.budget.consume(1)?;
            self.client
                .update_leverage(
                    &self.signer,
                    usize::try_from(asset.asset_index).map_err(|_| HyperliquidError::Numeric)?,
                    false,
                    leverage,
                    self.nonce.next(),
                    None,
                    None,
                )
                .await
                .map_err(|_| HyperliquidError::Sdk)?;
            self.budget.consume(INFO_ACTIVE_ASSET_WEIGHT)?;
            let confirmed = self
                .client
                .active_asset_data(account, symbol.to_owned())
                .await
                .map_err(|_| HyperliquidError::Snapshot)?;
            if !confirmed
                .leverage
                .leverage_type
                .eq_ignore_ascii_case("isolated")
                || confirmed.leverage.value != Decimal::from(leverage)
            {
                return Err(HyperliquidError::Snapshot);
            }
        }
        Ok(())
    }

    pub async fn cancel_direct(
        &self,
        symbol: &str,
        order_id: u64,
    ) -> Result<Vec<OrderResponseStatus>, HyperliquidError> {
        let asset = self.assets.get(symbol).ok_or(HyperliquidError::Symbol)?;
        self.budget.consume(1)?;
        self.client
            .cancel(
                &self.signer,
                BatchCancel {
                    cancels: vec![Cancel {
                        asset: usize::try_from(asset.asset_index)
                            .map_err(|_| HyperliquidError::Numeric)?,
                        oid: order_id,
                    }],
                },
                self.nonce.next(),
                None,
                None,
            )
            .await
            .map_err(|_| HyperliquidError::Sdk)
    }

    pub async fn account_snapshot(
        &self,
        execution_account: &str,
    ) -> Result<AccountSnapshot, HyperliquidError> {
        let account = parse_account(execution_account)?;
        self.budget
            .consume(INFO_USER_MODE_WEIGHT + INFO_ACCOUNT_STATE_WEIGHT * 2)?;
        let abstraction_mode = self
            .client
            .abstraction_mode(account)
            .await
            .map_err(|_| HyperliquidError::Snapshot)?;
        let state = self
            .client
            .clearinghouse_state(account, None)
            .await
            .map_err(|_| HyperliquidError::Sdk)?;
        let mut positions = BTreeMap::new();
        for asset in state.asset_positions {
            let position = asset.position;
            if !ALLOWED_SYMBOLS.contains(&position.coin.as_str()) || position.szi.is_zero() {
                continue;
            }
            let quantity_e8 = decimal_to_fixed(position.szi, 8)?;
            let notional_micro_usdc = decimal_to_fixed(position.position_value.abs(), 6)?;
            let snapshot = PositionSnapshot {
                symbol: position.coin.clone(),
                quantity_e8,
                notional_micro_usdc,
                entry_price_micro_usdc: position
                    .entry_px
                    .map(|value| decimal_to_fixed(value, 6))
                    .transpose()?,
                unrealized_pnl_micro_usdc: decimal_to_fixed(position.unrealized_pnl, 6)?,
                isolated: position.leverage.is_isolated(),
                leverage: position.leverage.value,
            };
            positions.insert(position.coin, snapshot);
        }
        // Hyperliquid exposes unified-account and portfolio-margin balances and
        // holds through spotClearinghouseState; their per-DEX margin summaries
        // are not a valid source of account collateral.
        let spot_balances = if abstraction_mode.is_standard() {
            Vec::new()
        } else {
            self.client
                .user_balances(account)
                .await
                .map_err(|_| HyperliquidError::Snapshot)?
        };
        let (equity_micro_usdc, withdrawable_micro_usdc) = account_collateral(
            abstraction_mode,
            state.margin_summary.account_value,
            state.withdrawable,
            &spot_balances,
        )?;
        Ok(AccountSnapshot {
            venue_time_ms: state.time,
            equity_micro_usdc,
            withdrawable_micro_usdc,
            positions,
        })
    }

    pub async fn market_snapshots(
        &self,
        books: Vec<BookSnapshot>,
        now_ms: u64,
    ) -> Result<BTreeMap<String, MarketSnapshot>, HyperliquidError> {
        self.budget.consume(INFO_META_WEIGHT)?;
        let raw = self
            .client
            .meta_and_asset_ctxs(None)
            .await
            .map_err(|_| HyperliquidError::Sdk)?;
        let values = raw.as_array().ok_or(HyperliquidError::Snapshot)?;
        if values.len() != 2 {
            return Err(HyperliquidError::Snapshot);
        }
        let contexts = serde_json::from_value::<Vec<PerpAssetCtx>>(values[1].clone())
            .map_err(|_| HyperliquidError::Snapshot)?;
        let books = books
            .into_iter()
            .map(|book| (book.symbol.clone(), book))
            .collect::<BTreeMap<_, _>>();
        let mut snapshots = BTreeMap::new();
        for symbol in ALLOWED_SYMBOLS {
            let asset = self.assets.get(symbol).ok_or(HyperliquidError::Metadata)?;
            let context = contexts
                .get(usize::try_from(asset.asset_index).map_err(|_| HyperliquidError::Snapshot)?)
                .ok_or(HyperliquidError::Snapshot)?;
            snapshots.insert(
                symbol.to_owned(),
                market_snapshot_from_parts(
                    asset,
                    context,
                    books.get(symbol).ok_or(HyperliquidError::Book)?.clone(),
                    now_ms,
                )?,
            );
        }
        Ok(snapshots)
    }

    /// Fetches the target market metadata first and its L2 book last so an IOC
    /// can be normalized and checked against dispatch-time liquidity.
    pub async fn fresh_market_snapshot(
        &self,
        symbol: &str,
        now_ms: u64,
    ) -> Result<MarketSnapshot, HyperliquidError> {
        let asset = self.assets.get(symbol).ok_or(HyperliquidError::Symbol)?;
        self.budget.consume(INFO_META_WEIGHT)?;
        let raw = self
            .client
            .meta_and_asset_ctxs(None)
            .await
            .map_err(|_| HyperliquidError::Sdk)?;
        let values = raw.as_array().ok_or(HyperliquidError::Snapshot)?;
        if values.len() != 2 {
            return Err(HyperliquidError::Snapshot);
        }
        let contexts = serde_json::from_value::<Vec<PerpAssetCtx>>(values[1].clone())
            .map_err(|_| HyperliquidError::Snapshot)?;
        let context = contexts
            .get(usize::try_from(asset.asset_index).map_err(|_| HyperliquidError::Snapshot)?)
            .ok_or(HyperliquidError::Snapshot)?;

        self.budget.consume(INFO_L2_WEIGHT)?;
        let snapshot = self
            .client
            .l2_book(symbol.to_owned(), None, None)
            .await
            .map_err(|_| HyperliquidError::Sdk)?;
        let book = BookSnapshot::try_from_parts(snapshot.coin, snapshot.time, &snapshot.levels)?;
        let observed_at_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| HyperliquidError::Snapshot)?
                .as_millis(),
        )
        .map_err(|_| HyperliquidError::Snapshot)?
        .max(now_ms);
        market_snapshot_from_parts(asset, context, book, observed_at_ms)
    }

    pub async fn recent_candles(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError> {
        let mut output = serde_json::Map::new();
        for symbol in ALLOWED_SYMBOLS {
            self.budget.consume(INFO_CANDLE_WEIGHT)?;
            let candles = self
                .client
                .candle_snapshot(
                    symbol.to_owned(),
                    CandleInterval::FifteenMinutes,
                    start_time_ms,
                    end_time_ms,
                )
                .await
                .map_err(|_| HyperliquidError::Sdk)?;
            output.insert(
                symbol.to_owned(),
                serde_json::to_value(candles).map_err(|_| HyperliquidError::Snapshot)?,
            );
        }
        Ok(Value::Object(output))
    }

    pub async fn fills_since(
        &self,
        execution_account: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError> {
        let account = parse_account(execution_account)?;
        self.budget.consume(INFO_USER_HISTORY_WEIGHT)?;
        let fills = self
            .client
            .user_fills_by_time(account, start_time_ms, Some(end_time_ms))
            .await
            .map_err(|_| HyperliquidError::Sdk)?
            .into_iter()
            .filter(|fill| ALLOWED_SYMBOLS.contains(&fill.coin.as_str()))
            .collect::<Vec<_>>();
        serde_json::to_value(fills).map_err(|_| HyperliquidError::Snapshot)
    }

    pub async fn funding_since(
        &self,
        execution_account: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Value, HyperliquidError> {
        let account = parse_account(execution_account)?;
        self.budget.consume(INFO_USER_HISTORY_WEIGHT)?;
        let funding = self
            .client
            .user_funding(account, start_time_ms, Some(end_time_ms))
            .await
            .map_err(|_| HyperliquidError::Sdk)?
            .into_iter()
            .filter(|entry| ALLOWED_SYMBOLS.contains(&entry.delta.coin.as_str()))
            .collect::<Vec<_>>();
        serde_json::to_value(funding).map_err(|_| HyperliquidError::Snapshot)
    }
}

fn market_snapshot_from_parts(
    asset: &CoreAsset,
    context: &PerpAssetCtx,
    book: BookSnapshot,
    now_ms: u64,
) -> Result<MarketSnapshot, HyperliquidError> {
    let bid = book.bids.first().ok_or(HyperliquidError::Book)?;
    let ask = book.asks.first().ok_or(HyperliquidError::Book)?;
    let bid_price = decimal_string_to_fixed(&bid.price, 6)?;
    let ask_price = decimal_string_to_fixed(&ask.price, 6)?;
    if bid_price <= 0 || ask_price < bid_price || now_ms < book.venue_time_ms {
        return Err(HyperliquidError::Book);
    }
    let midpoint = bid_price
        .checked_add(ask_price)
        .ok_or(HyperliquidError::Numeric)?
        / 2;
    let spread_bps = i64::try_from(
        i128::from(ask_price - bid_price)
            .checked_mul(10_000)
            .ok_or(HyperliquidError::Numeric)?
            / i128::from(midpoint),
    )
    .map_err(|_| HyperliquidError::Numeric)?;
    let bid_depth = book.marketable_depth_micro_usdc(Side::Sell)?;
    let ask_depth = book.marketable_depth_micro_usdc(Side::Buy)?;
    Ok(MarketSnapshot {
        market: crate::MarketState {
            symbol: asset.symbol.clone(),
            mark_price_micro_usdc: decimal_to_fixed(context.mark_px, 6)?,
            oracle_price_micro_usdc: decimal_to_fixed(context.oracle_px, 6)?,
            spread_bps: u16::try_from(spread_bps).map_err(|_| HyperliquidError::Numeric)?,
            book_age_seconds: u16::try_from((now_ms - book.venue_time_ms) / 1_000)
                .unwrap_or(u16::MAX),
            ask_depth_micro_usdc: ask_depth,
            bid_depth_micro_usdc: bid_depth,
            size_decimals: asset.size_decimals,
            delisted: false,
        },
        book,
    })
}

fn display_safe_venue_rejection(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("immediately match") || message.contains("would not fill") {
        "ioc_would_not_fill"
    } else if message.contains("insufficient margin") {
        "insufficient_margin"
    } else if message.contains("reduce only") || message.contains("reduce-only") {
        "invalid_reduce_only"
    } else if message.contains("minimum") && message.contains("notional") {
        "minimum_notional"
    } else if message.contains("price") {
        "invalid_price"
    } else if message.contains("size") {
        "invalid_size"
    } else {
        "venue_rejected"
    }
}

pub fn hyperliquid_api_wallet_address(
    api_wallet_key: &[u8; 32],
) -> Result<String, HyperliquidError> {
    let encoded_key = Zeroizing::new(format!("0x{}", hex::encode(api_wallet_key)));
    let signer = encoded_key
        .parse::<PrivateKeySigner>()
        .map_err(|_| HyperliquidError::Wallet)?;
    Ok(signer.address().to_string())
}

pub struct HyperliquidBookStream {
    info: HttpClient,
    stream: WebSocket,
    budget: Arc<RequestBudget>,
    network: HyperliquidNetwork,
}

impl std::fmt::Debug for HyperliquidBookStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HyperliquidBookStream")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl HyperliquidBookStream {
    pub fn connect_testnet() -> Result<Self, HyperliquidError> {
        Ok(Self::connect(HyperliquidNetwork::Testnet))
    }

    pub fn connect_mainnet() -> Result<Self, HyperliquidError> {
        Ok(Self::connect(HyperliquidNetwork::Mainnet))
    }

    fn connect(network: HyperliquidNetwork) -> Self {
        let budget = Arc::new(RequestBudget::new());
        let stream = match network {
            HyperliquidNetwork::Testnet => hypercore::testnet_ws(),
            HyperliquidNetwork::Mainnet => hypercore::mainnet_ws(),
        };
        for symbol in ALLOWED_SYMBOLS {
            stream.subscribe(Subscription::L2Book {
                coin: symbol.to_owned(),
                n_sig_figs: None,
                mantissa: None,
                fast: false,
            });
        }
        Self {
            info: match network {
                HyperliquidNetwork::Testnet => hypercore::testnet(),
                HyperliquidNetwork::Mainnet => hypercore::mainnet(),
            },
            stream,
            budget,
            network,
        }
    }

    pub async fn next_snapshot(&mut self) -> Result<BookSnapshot, HyperliquidError> {
        loop {
            match self.stream.next().await {
                Some(Event::Message(Incoming::L2Book(message))) => {
                    return BookSnapshot::try_from_parts(
                        message.coin,
                        message.time,
                        &message.levels,
                    );
                }
                None => return Err(HyperliquidError::StreamClosed),
                Some(_) => {}
            }
        }
    }

    pub async fn reconcile(&self) -> Result<Vec<BookSnapshot>, HyperliquidError> {
        let mut snapshots = Vec::with_capacity(ALLOWED_SYMBOLS.len());
        for symbol in ALLOWED_SYMBOLS {
            self.budget.consume(INFO_L2_WEIGHT)?;
            let snapshot = self
                .info
                .l2_book(symbol.to_owned(), None, None)
                .await
                .map_err(|_| HyperliquidError::Sdk)?;
            snapshots.push(BookSnapshot::try_from_parts(
                snapshot.coin,
                snapshot.time,
                &snapshot.levels,
            )?);
        }
        Ok(snapshots)
    }
}

impl BookSnapshot {
    fn try_from_parts<T>(
        symbol: String,
        venue_time_ms: u64,
        levels: &[Vec<T>],
    ) -> Result<Self, HyperliquidError>
    where
        T: VenueBookLevel,
    {
        if !ALLOWED_SYMBOLS.contains(&symbol.as_str()) || levels.len() != 2 {
            return Err(HyperliquidError::Book);
        }
        let bids = levels[0].iter().map(VenueBookLevel::normalized).collect();
        let asks = levels[1].iter().map(VenueBookLevel::normalized).collect();
        Ok(Self {
            symbol,
            venue_time_ms,
            bids,
            asks,
        })
    }
}

trait VenueBookLevel {
    fn normalized(&self) -> BookLevel;
}

impl VenueBookLevel for hypersdk::hypercore::types::BookLevel {
    fn normalized(&self) -> BookLevel {
        BookLevel {
            price: self.px.normalize().to_string(),
            size: self.sz.normalize().to_string(),
            order_count: u64::try_from(self.n).unwrap_or(u64::MAX),
        }
    }
}

fn discover_core_assets(
    markets: &[PerpMarket],
) -> Result<BTreeMap<String, CoreAsset>, HyperliquidError> {
    discover_core_assets_from(markets.iter().map(|market| {
        (
            market.name.as_str(),
            market.index,
            market.sz_decimals,
            market.max_leverage,
        )
    }))
}

fn discover_core_assets_from<'a>(
    markets: impl IntoIterator<Item = (&'a str, usize, i64, u64)>,
) -> Result<BTreeMap<String, CoreAsset>, HyperliquidError> {
    let mut assets = BTreeMap::new();
    for (name, index, size_decimals, max_leverage) in markets {
        if ALLOWED_SYMBOLS.contains(&name) {
            let asset_index = u32::try_from(index).map_err(|_| HyperliquidError::Metadata)?;
            let size_decimals =
                u8::try_from(size_decimals).map_err(|_| HyperliquidError::Metadata)?;
            let max_leverage =
                u32::try_from(max_leverage).map_err(|_| HyperliquidError::Metadata)?;
            assets.insert(
                name.to_owned(),
                CoreAsset {
                    symbol: name.to_owned(),
                    asset_index,
                    size_decimals,
                    max_leverage,
                },
            );
        }
    }
    if assets.len() != ALLOWED_SYMBOLS.len() {
        return Err(HyperliquidError::Metadata);
    }
    Ok(assets)
}

fn build_ioc_request(
    order: &OrderDecision,
    assets: &BTreeMap<String, CoreAsset>,
    cloid: Cloid,
) -> Result<OrderRequest, HyperliquidError> {
    let asset = assets.get(&order.symbol).ok_or(HyperliquidError::Symbol)?;
    let limit_price = fixed_decimal(order.limit_price_micro_usdc, 6)?;
    let size = fixed_decimal(order.quantity_e8, 8)?;
    let limit_px = limit_price
        .parse::<Decimal>()
        .map_err(|_| HyperliquidError::Numeric)?;
    let sz = size
        .parse::<Decimal>()
        .map_err(|_| HyperliquidError::Numeric)?;
    if decimal_places(&size) > usize::from(asset.size_decimals) {
        return Err(HyperliquidError::Numeric);
    }
    Ok(OrderRequest {
        asset: usize::try_from(asset.asset_index).map_err(|_| HyperliquidError::Numeric)?,
        is_buy: order.side == Side::Buy,
        reduce_only: order.reduce_only,
        limit_px,
        sz,
        cloid,
        order_type: OrderTypePlacement::Limit {
            tif: TimeInForce::Ioc,
        },
    })
}

fn fixed_decimal(value: i64, scale: u8) -> Result<String, HyperliquidError> {
    if value <= 0 {
        return Err(HyperliquidError::Numeric);
    }
    let divisor = 10_i64
        .checked_pow(u32::from(scale))
        .ok_or(HyperliquidError::Numeric)?;
    let whole = value / divisor;
    let fraction = value % divisor;
    if fraction == 0 {
        return Ok(whole.to_string());
    }
    let mut encoded = format!("{whole}.{fraction:0width$}", width = usize::from(scale));
    while encoded.ends_with('0') {
        encoded.pop();
    }
    Ok(encoded)
}

fn decimal_places(value: &str) -> usize {
    value
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len())
}

fn parse_account(value: &str) -> Result<Address, HyperliquidError> {
    value.parse().map_err(|_| HyperliquidError::Wallet)
}

fn decimal_to_fixed(value: Decimal, scale: u8) -> Result<i64, HyperliquidError> {
    decimal_string_to_fixed(&value.normalize().to_string(), scale)
}

fn account_collateral(
    mode: AbstractionMode,
    standard_equity: Decimal,
    standard_withdrawable: Decimal,
    spot_balances: &[UserBalance],
) -> Result<(i64, i64), HyperliquidError> {
    if mode.is_standard() {
        return Ok((
            decimal_to_fixed(standard_equity, 6)?,
            decimal_to_fixed(standard_withdrawable, 6)?,
        ));
    }
    let usdc = spot_balances
        .iter()
        .find(|balance| balance.coin == "USDC")
        .ok_or(HyperliquidError::Snapshot)?;
    if usdc.total.is_sign_negative() || usdc.hold.is_sign_negative() || usdc.hold > usdc.total {
        return Err(HyperliquidError::Snapshot);
    }
    Ok((
        decimal_to_fixed(usdc.total, 6)?,
        decimal_to_fixed(usdc.available(), 6)?,
    ))
}

fn decimal_string_to_fixed(value: &str, scale: u8) -> Result<i64, HyperliquidError> {
    let negative = value.starts_with('-');
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(HyperliquidError::Numeric);
    }
    let scale = usize::from(scale);
    if fraction.len() > scale && fraction[scale..].bytes().any(|byte| byte != b'0') {
        return Err(HyperliquidError::Numeric);
    }
    let whole = whole
        .parse::<i128>()
        .map_err(|_| HyperliquidError::Numeric)?;
    let mut fractional = fraction[..fraction.len().min(scale)].to_owned();
    fractional.extend(std::iter::repeat_n(
        '0',
        scale.saturating_sub(fractional.len()),
    ));
    let fractional = if fractional.is_empty() {
        0
    } else {
        fractional
            .parse::<i128>()
            .map_err(|_| HyperliquidError::Numeric)?
    };
    let multiplier = 10_i128
        .checked_pow(u32::try_from(scale).map_err(|_| HyperliquidError::Numeric)?)
        .ok_or(HyperliquidError::Numeric)?;
    let fixed = whole
        .checked_mul(multiplier)
        .and_then(|value| value.checked_add(fractional))
        .ok_or(HyperliquidError::Numeric)?;
    i64::try_from(if negative { -fixed } else { fixed }).map_err(|_| HyperliquidError::Numeric)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_indices_are_discovered_in_venue_order() -> Result<(), HyperliquidError> {
        let assets = discover_core_assets_from([
            ("DOGE", 0, 0, 10),
            ("SOL", 1, 2, 20),
            ("BTC", 2, 5, 40),
            ("ETH", 3, 4, 25),
        ])?;
        assert_eq!(assets["SOL"].asset_index, 1);
        assert_eq!(assets["BTC"].asset_index, 2);
        assert_eq!(assets["ETH"].asset_index, 3);
        assert_eq!(assets["BTC"].max_leverage, 40);
        Ok(())
    }

    #[test]
    fn order_builder_is_ioc_only() -> Result<(), HyperliquidError> {
        let assets =
            discover_core_assets_from([("BTC", 0, 5, 40), ("ETH", 1, 4, 25), ("SOL", 2, 2, 20)])?;
        let request = build_ioc_request(
            &OrderDecision {
                symbol: "BTC".into(),
                side: Side::Buy,
                quantity_e8: 10_000,
                limit_price_micro_usdc: 100_125_000_000,
                actual_notional_micro_usdc: 10_012_500,
                effective_notional_bps: 100,
                reduce_only: false,
            },
            &assets,
            Cloid::default(),
        )?;
        assert!(request.is_buy);
        assert_eq!(request.asset, 0);
        assert!(matches!(
            request.order_type,
            OrderTypePlacement::Limit {
                tif: TimeInForce::Ioc
            }
        ));
        Ok(())
    }

    #[test]
    fn ioc_limit_crosses_the_opposing_top_of_book() -> Result<(), HyperliquidError> {
        let book = BookSnapshot {
            symbol: "BTC".into(),
            venue_time_ms: 1,
            bids: vec![BookLevel {
                price: "63443".into(),
                size: "0.0268".into(),
                order_count: 2,
            }],
            asks: vec![BookLevel {
                price: "63469".into(),
                size: "0.01265".into(),
                order_count: 2,
            }],
        };
        assert_eq!(
            book.marketable_limit_price_micro_usdc(Side::Buy)?,
            63_469_000_000
        );
        assert_eq!(
            book.marketable_limit_price_micro_usdc(Side::Sell)?,
            63_443_000_000
        );
        Ok(())
    }

    #[test]
    fn market_ioc_uses_displayed_depth_inside_the_five_percent_ceiling()
    -> Result<(), HyperliquidError> {
        let book = BookSnapshot {
            symbol: "BTC".into(),
            venue_time_ms: 1,
            bids: vec![
                BookLevel {
                    price: "63477".into(),
                    size: "0.00024".into(),
                    order_count: 1,
                },
                BookLevel {
                    price: "63462".into(),
                    size: "0.00155".into(),
                    order_count: 1,
                },
                BookLevel {
                    price: "50000".into(),
                    size: "10".into(),
                    order_count: 1,
                },
            ],
            asks: vec![
                BookLevel {
                    price: "63490".into(),
                    size: "0.00024".into(),
                    order_count: 1,
                },
                BookLevel {
                    price: "63498".into(),
                    size: "0.00132".into(),
                    order_count: 1,
                },
                BookLevel {
                    price: "70000".into(),
                    size: "10".into(),
                    order_count: 1,
                },
            ],
        };
        assert_eq!(
            book.marketable_limit_price_micro_usdc(Side::Buy)?,
            63_498_000_000
        );
        assert_eq!(
            book.marketable_limit_price_micro_usdc(Side::Sell)?,
            63_462_000_000
        );
        assert_eq!(book.marketable_depth_micro_usdc(Side::Buy)?, 99_054_960);
        assert_eq!(book.marketable_depth_micro_usdc(Side::Sell)?, 113_600_580);
        Ok(())
    }

    #[test]
    fn venue_errors_are_reduced_to_display_safe_reasons() {
        assert_eq!(
            display_safe_venue_rejection(
                "Order could not immediately match against any resting orders."
            ),
            "ioc_would_not_fill"
        );
        assert_eq!(
            display_safe_venue_rejection("Insufficient margin to place order."),
            "insufficient_margin"
        );
        assert_eq!(
            display_safe_venue_rejection("internal detail"),
            "venue_rejected"
        );
    }

    #[test]
    fn standard_account_uses_only_perpetual_collateral() -> Result<(), HyperliquidError> {
        let spot = serde_json::from_value::<UserBalance>(json!({
            "coin": "USDC",
            "token": 0,
            "hold": "1.0",
            "total": "1001.473289",
            "entryNtl": "0.0"
        }))
        .map_err(|_| HyperliquidError::Snapshot)?;
        assert_eq!(
            account_collateral(
                AbstractionMode::Standard,
                "25.5".parse().map_err(|_| HyperliquidError::Numeric)?,
                "20.25".parse().map_err(|_| HyperliquidError::Numeric)?,
                &[spot],
            )?,
            (25_500_000, 20_250_000)
        );
        Ok(())
    }

    #[test]
    fn unified_account_uses_verified_usdc_total_and_available() -> Result<(), HyperliquidError> {
        let spot = serde_json::from_value::<UserBalance>(json!({
            "coin": "USDC",
            "token": 0,
            "hold": "1.0",
            "total": "1001.473289",
            "entryNtl": "0.0"
        }))
        .map_err(|_| HyperliquidError::Snapshot)?;
        assert_eq!(
            account_collateral(
                AbstractionMode::UnifiedAccount,
                Decimal::ZERO,
                Decimal::ZERO,
                &[spot],
            )?,
            (1_001_473_289, 1_000_473_289)
        );
        Ok(())
    }

    #[test]
    fn unified_account_fails_closed_without_valid_usdc() {
        assert!(matches!(
            account_collateral(
                AbstractionMode::UnifiedAccount,
                Decimal::ZERO,
                Decimal::ZERO,
                &[],
            ),
            Err(HyperliquidError::Snapshot)
        ));
    }
}
