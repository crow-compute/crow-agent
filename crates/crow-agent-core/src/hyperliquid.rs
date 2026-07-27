use crate::{OrderDecision, Side};
use crow_agent_protocol::ALLOWED_SYMBOLS;
use futures_util::StreamExt;
use hypersdk::{
    Decimal,
    hypercore::{
        self, Cloid, HttpClient, NonceHandler, PerpMarket, PrivateKeySigner, WebSocket,
        types::{
            BatchCancel, BatchOrder, Cancel, Incoming, OrderGrouping, OrderRequest,
            OrderResponseStatus, OrderTypePlacement, Subscription, TimeInForce,
        },
        ws::Event,
    },
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use zeroize::Zeroizing;

const REST_WEIGHT_PER_MINUTE: u16 = 1_000;
const INFO_META_WEIGHT: u16 = 20;
const INFO_L2_WEIGHT: u16 = 2;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreAsset {
    pub symbol: String,
    pub asset_index: u32,
    pub size_decimals: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookLevel {
    pub price: String,
    pub size: String,
    pub order_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookSnapshot {
    pub symbol: String,
    pub venue_time_ms: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

impl BookSnapshot {
    #[must_use]
    pub fn is_fresh_at(&self, now_ms: u64, maximum_age: Duration) -> bool {
        now_ms >= self.venue_time_ms
            && now_ms - self.venue_time_ms
                <= u64::try_from(maximum_age.as_millis()).unwrap_or(u64::MAX)
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
}

impl std::fmt::Debug for HyperliquidVenue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HyperliquidVenue")
            .field("network", &"testnet")
            .field("assets", &self.assets)
            .field("api_wallet", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl HyperliquidVenue {
    pub async fn connect_testnet(
        api_wallet_key: &Zeroizing<[u8; 32]>,
    ) -> Result<Self, HyperliquidError> {
        let encoded_key = Zeroizing::new(format!("0x{}", hex::encode(api_wallet_key.as_ref())));
        let signer = encoded_key
            .parse::<PrivateKeySigner>()
            .map_err(|_| HyperliquidError::Wallet)?;
        let budget = Arc::new(RequestBudget::new());
        budget.consume(INFO_META_WEIGHT)?;
        let client = hypercore::testnet();
        let markets = client.perps().await.map_err(|_| HyperliquidError::Sdk)?;
        let assets = discover_core_assets(&markets)?;
        Ok(Self {
            client,
            signer,
            nonce: NonceHandler::default(),
            assets,
            budget,
        })
    }

    #[must_use]
    pub fn assets(&self) -> &BTreeMap<String, CoreAsset> {
        &self.assets
    }

    pub async fn submit_ioc(
        &self,
        order: &OrderDecision,
    ) -> Result<Vec<OrderResponseStatus>, HyperliquidError> {
        self.budget.consume(1)?;
        let request = build_ioc_request(order, &self.assets)?;
        self.client
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
            .map_err(|_| HyperliquidError::Sdk)
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
}

pub struct HyperliquidBookStream {
    info: HttpClient,
    stream: WebSocket,
    budget: Arc<RequestBudget>,
}

impl std::fmt::Debug for HyperliquidBookStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HyperliquidBookStream")
            .field("network", &"testnet")
            .finish_non_exhaustive()
    }
}

impl HyperliquidBookStream {
    pub fn connect_testnet() -> Result<Self, HyperliquidError> {
        let budget = Arc::new(RequestBudget::new());
        let stream = hypercore::testnet_ws();
        for symbol in ALLOWED_SYMBOLS {
            stream.subscribe(Subscription::L2Book {
                coin: symbol.to_owned(),
                n_sig_figs: None,
                mantissa: None,
                fast: false,
            });
        }
        Ok(Self {
            info: hypercore::testnet(),
            stream,
            budget,
        })
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
    discover_core_assets_from(
        markets
            .iter()
            .map(|market| (market.name.as_str(), market.index, market.sz_decimals)),
    )
}

fn discover_core_assets_from<'a>(
    markets: impl IntoIterator<Item = (&'a str, usize, i64)>,
) -> Result<BTreeMap<String, CoreAsset>, HyperliquidError> {
    let mut assets = BTreeMap::new();
    for (name, index, size_decimals) in markets {
        if ALLOWED_SYMBOLS.contains(&name) {
            let asset_index = u32::try_from(index).map_err(|_| HyperliquidError::Metadata)?;
            let size_decimals =
                u8::try_from(size_decimals).map_err(|_| HyperliquidError::Metadata)?;
            assets.insert(
                name.to_owned(),
                CoreAsset {
                    symbol: name.to_owned(),
                    asset_index,
                    size_decimals,
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
        cloid: Cloid::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_indices_are_discovered_in_venue_order() -> Result<(), HyperliquidError> {
        let assets = discover_core_assets_from([
            ("DOGE", 0, 0),
            ("SOL", 1, 2),
            ("BTC", 2, 5),
            ("ETH", 3, 4),
        ])?;
        assert_eq!(assets["SOL"].asset_index, 1);
        assert_eq!(assets["BTC"].asset_index, 2);
        assert_eq!(assets["ETH"].asset_index, 3);
        Ok(())
    }

    #[test]
    fn order_builder_is_ioc_only() -> Result<(), HyperliquidError> {
        let assets = discover_core_assets_from([("BTC", 0, 5), ("ETH", 1, 4), ("SOL", 2, 2)])?;
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
}
