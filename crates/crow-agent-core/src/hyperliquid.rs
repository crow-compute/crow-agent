use crate::{OrderDecision, Side};
use crow_agent_protocol::ALLOWED_SYMBOLS;
use ethers::signers::LocalWallet;
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeResponseStatus, InfoClient, Message, Meta, Subscription,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use zeroize::Zeroizing;

const REST_WEIGHT_PER_MINUTE: u16 = 1_000;
const INFO_META_WEIGHT: u16 = 20;
const INFO_L2_WEIGHT: u16 = 2;

#[derive(Debug, Error)]
pub enum HyperliquidError {
    #[error("Hyperliquid SDK request failed")]
    Sdk(#[from] hyperliquid_rust_sdk::Error),
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
        if state.opened_at.elapsed() >= Duration::from_secs(60) {
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
    exchange: ExchangeClient,
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
        let wallet = LocalWallet::from_bytes(api_wallet_key.as_ref())
            .map_err(|_| HyperliquidError::Wallet)?;
        let budget = Arc::new(RequestBudget::new());
        budget.consume(INFO_META_WEIGHT)?;
        let exchange =
            ExchangeClient::new(None, wallet, Some(BaseUrl::Testnet), None, None).await?;
        let assets = discover_core_assets(&exchange.meta)?;
        Ok(Self {
            exchange,
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
    ) -> Result<ExchangeResponseStatus, HyperliquidError> {
        self.budget.consume(1)?;
        let request = build_ioc_request(order, &self.assets)?;
        self.exchange.order(request, None).await.map_err(Into::into)
    }

    pub async fn cancel_direct(
        &self,
        symbol: &str,
        order_id: u64,
    ) -> Result<ExchangeResponseStatus, HyperliquidError> {
        if !self.assets.contains_key(symbol) {
            return Err(HyperliquidError::Symbol);
        }
        self.budget.consume(1)?;
        self.exchange
            .cancel(
                ClientCancelRequest {
                    asset: symbol.to_owned(),
                    oid: order_id,
                },
                None,
            )
            .await
            .map_err(Into::into)
    }
}

pub struct HyperliquidBookStream {
    info: InfoClient,
    receiver: UnboundedReceiver<Message>,
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
    pub async fn connect_testnet() -> Result<Self, HyperliquidError> {
        let budget = Arc::new(RequestBudget::new());
        let mut info = InfoClient::with_reconnect(None, Some(BaseUrl::Testnet)).await?;
        let (sender, receiver) = unbounded_channel();
        for symbol in ALLOWED_SYMBOLS {
            info.subscribe(
                Subscription::L2Book {
                    coin: symbol.to_owned(),
                },
                sender.clone(),
            )
            .await?;
        }
        Ok(Self {
            info,
            receiver,
            budget,
        })
    }

    pub async fn next_snapshot(&mut self) -> Result<BookSnapshot, HyperliquidError> {
        loop {
            match self.receiver.recv().await {
                Some(Message::L2Book(message)) => {
                    return BookSnapshot::try_from_parts(
                        message.data.coin,
                        message.data.time,
                        &message.data.levels,
                    );
                }
                Some(Message::HyperliquidError(_)) | None => {
                    return Err(HyperliquidError::StreamClosed);
                }
                Some(_) => {}
            }
        }
    }

    pub async fn reconcile(&self) -> Result<Vec<BookSnapshot>, HyperliquidError> {
        let mut snapshots = Vec::with_capacity(ALLOWED_SYMBOLS.len());
        for symbol in ALLOWED_SYMBOLS {
            self.budget.consume(INFO_L2_WEIGHT)?;
            let snapshot = self.info.l2_snapshot(symbol.to_owned()).await?;
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

impl VenueBookLevel for hyperliquid_rust_sdk::Level {
    fn normalized(&self) -> BookLevel {
        BookLevel {
            price: self.px.clone(),
            size: self.sz.clone(),
            order_count: self.n,
        }
    }
}

impl VenueBookLevel for hyperliquid_rust_sdk::BookLevel {
    fn normalized(&self) -> BookLevel {
        BookLevel {
            price: self.px.clone(),
            size: self.sz.clone(),
            order_count: self.n,
        }
    }
}

fn discover_core_assets(meta: &Meta) -> Result<BTreeMap<String, CoreAsset>, HyperliquidError> {
    let mut assets = BTreeMap::new();
    for (index, asset) in meta.universe.iter().enumerate() {
        if ALLOWED_SYMBOLS.contains(&asset.name.as_str()) {
            let asset_index = u32::try_from(index).map_err(|_| HyperliquidError::Metadata)?;
            let size_decimals =
                u8::try_from(asset.sz_decimals).map_err(|_| HyperliquidError::Metadata)?;
            assets.insert(
                asset.name.clone(),
                CoreAsset {
                    symbol: asset.name.clone(),
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
) -> Result<ClientOrderRequest, HyperliquidError> {
    let asset = assets.get(&order.symbol).ok_or(HyperliquidError::Symbol)?;
    let limit_price = fixed_decimal(order.limit_price_micro_usdc, 6)?;
    let size = fixed_decimal(order.quantity_e8, 8)?;
    let limit_px = limit_price
        .parse::<f64>()
        .map_err(|_| HyperliquidError::Numeric)?;
    let sz = size.parse::<f64>().map_err(|_| HyperliquidError::Numeric)?;
    if decimal_places(&size) > usize::from(asset.size_decimals) {
        return Err(HyperliquidError::Numeric);
    }
    Ok(ClientOrderRequest {
        asset: order.symbol.clone(),
        is_buy: order.side == Side::Buy,
        reduce_only: order.reduce_only,
        limit_px,
        sz,
        cloid: None,
        order_type: ClientOrder::Limit(ClientLimit { tif: "Ioc".into() }),
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
    use hyperliquid_rust_sdk::AssetMeta;

    #[test]
    fn metadata_indices_are_discovered_in_venue_order() -> Result<(), HyperliquidError> {
        let meta = Meta {
            universe: vec![
                asset("DOGE", 0),
                asset("SOL", 2),
                asset("BTC", 5),
                asset("ETH", 4),
            ],
        };
        let assets = discover_core_assets(&meta)?;
        assert_eq!(assets["SOL"].asset_index, 1);
        assert_eq!(assets["BTC"].asset_index, 2);
        assert_eq!(assets["ETH"].asset_index, 3);
        Ok(())
    }

    #[test]
    fn order_builder_is_ioc_only() -> Result<(), HyperliquidError> {
        let assets = discover_core_assets(&Meta {
            universe: vec![asset("BTC", 5), asset("ETH", 4), asset("SOL", 2)],
        })?;
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
        assert_eq!(request.asset, "BTC");
        assert!(
            matches!(request.order_type, ClientOrder::Limit(ClientLimit { tif }) if tif == "Ioc")
        );
        Ok(())
    }

    fn asset(name: &str, size_decimals: u32) -> AssetMeta {
        AssetMeta {
            name: name.into(),
            sz_decimals: size_decimals,
        }
    }
}
