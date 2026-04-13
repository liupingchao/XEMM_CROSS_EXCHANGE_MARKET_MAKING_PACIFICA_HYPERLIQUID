use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::connector::binance::BinanceTrading;
use crate::connector::pacifica::{OrderSide as PacificaOrderSide, PacificaTrading};
use crate::strategy::OrderSide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakerExchangeKind {
    Pacifica,
    Binance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakerOrderSide {
    Buy,
    Sell,
}

impl MakerOrderSide {
    pub fn from_strategy_side(side: OrderSide) -> Self {
        match side {
            OrderSide::Buy => Self::Buy,
            OrderSide::Sell => Self::Sell,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MakerSymbolInfo {
    pub tick_size: f64,
    pub lot_size: f64,
}

#[derive(Debug, Clone)]
pub struct MakerOrderData {
    pub order_id: Option<String>,
    pub client_order_id: Option<String>,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MakerOpenOrder {
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub symbol: String,
    pub side: MakerOrderSide,
    pub price: f64,
    pub initial_amount: f64,
    pub filled_amount: f64,
}

#[derive(Debug, Clone)]
pub struct MakerPosition {
    pub symbol: String,
    pub side: MakerOrderSide,
    pub amount: f64,
}

impl MakerPosition {
    pub fn signed_amount(&self) -> f64 {
        match self.side {
            MakerOrderSide::Buy => self.amount,
            MakerOrderSide::Sell => -self.amount,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MakerTradeHistoryItem {
    pub order_id: Option<String>,
    pub client_order_id: Option<String>,
    pub symbol: String,
    pub amount: f64,
    pub entry_price: f64,
    pub fee: f64,
    pub is_maker_fill: bool,
    pub created_at_ms: u64,
}

#[async_trait]
pub trait MakerExchange: Send + Sync {
    fn kind(&self) -> MakerExchangeKind;
    fn name(&self) -> &'static str;

    async fn get_symbol_info(&self, symbol: &str) -> Result<MakerSymbolInfo>;
    async fn get_best_bid_ask_rest(&self, symbol: &str, agg_level: u32) -> Result<Option<(f64, f64)>>;
    async fn place_limit_order(
        &self,
        symbol: &str,
        side: MakerOrderSide,
        size: f64,
        price: f64,
        current_bid: Option<f64>,
        current_ask: Option<f64>,
    ) -> Result<MakerOrderData>;
    async fn cancel_all_orders(&self, symbol: Option<&str>) -> Result<u32>;
    async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<MakerOpenOrder>>;
    async fn get_positions(&self, symbol: Option<&str>) -> Result<Vec<MakerPosition>>;
    async fn get_trade_history(
        &self,
        symbol: Option<&str>,
        limit: Option<u32>,
        start_time: Option<u64>,
        end_time: Option<u64>,
    ) -> Result<Vec<MakerTradeHistoryItem>>;
}

pub struct PacificaMakerExchange {
    inner: Arc<PacificaTrading>,
}

impl PacificaMakerExchange {
    pub fn new(inner: Arc<PacificaTrading>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl MakerExchange for PacificaMakerExchange {
    fn kind(&self) -> MakerExchangeKind {
        MakerExchangeKind::Pacifica
    }

    fn name(&self) -> &'static str {
        "Pacifica"
    }

    async fn get_symbol_info(&self, symbol: &str) -> Result<MakerSymbolInfo> {
        let market_info = self.inner.get_market_info().await?;
        let symbol_info = market_info
            .get(symbol)
            .with_context(|| format!("Symbol {symbol} not found in Pacifica market info"))?;
        let tick_size = symbol_info.tick_size.parse::<f64>().context("Failed to parse Pacifica tick_size")?;
        let lot_size = symbol_info.lot_size.parse::<f64>().context("Failed to parse Pacifica lot_size")?;
        Ok(MakerSymbolInfo { tick_size, lot_size })
    }

    async fn get_best_bid_ask_rest(&self, symbol: &str, agg_level: u32) -> Result<Option<(f64, f64)>> {
        self.inner.get_best_bid_ask_rest(symbol, agg_level).await
    }

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: MakerOrderSide,
        size: f64,
        price: f64,
        current_bid: Option<f64>,
        current_ask: Option<f64>,
    ) -> Result<MakerOrderData> {
        let pac_side = match side {
            MakerOrderSide::Buy => PacificaOrderSide::Buy,
            MakerOrderSide::Sell => PacificaOrderSide::Sell,
        };

        let order = self
            .inner
            .place_limit_order(
                symbol,
                pac_side,
                size,
                Some(price),
                0.0,
                current_bid,
                current_ask,
            )
            .await?;

        Ok(MakerOrderData {
            order_id: order.order_id.map(|v| v.to_string()),
            client_order_id: order.client_order_id,
            symbol: order.symbol,
        })
    }

    async fn cancel_all_orders(&self, symbol: Option<&str>) -> Result<u32> {
        self.inner.cancel_all_orders(false, symbol, false).await
    }

    async fn get_open_orders(&self, _symbol: Option<&str>) -> Result<Vec<MakerOpenOrder>> {
        let orders = self.inner.get_open_orders().await?;

        Ok(orders
            .into_iter()
            .map(|o| MakerOpenOrder {
                order_id: Some(o.order_id.to_string()),
                client_order_id: o.client_order_id,
                symbol: o.symbol,
                side: if matches!(o.side.as_str(), "bid" | "buy") {
                    MakerOrderSide::Buy
                } else {
                    MakerOrderSide::Sell
                },
                price: fast_float::parse(&o.price).unwrap_or(0.0),
                initial_amount: fast_float::parse(&o.initial_amount).unwrap_or(0.0),
                filled_amount: fast_float::parse(&o.filled_amount).unwrap_or(0.0),
            })
            .collect())
    }

    async fn get_positions(&self, symbol: Option<&str>) -> Result<Vec<MakerPosition>> {
        let positions = self.inner.get_positions().await?;
        let mut mapped: Vec<MakerPosition> = positions
            .into_iter()
            .filter(|p| symbol.map(|s| p.symbol == s).unwrap_or(true))
            .map(|p| MakerPosition {
                symbol: p.symbol,
                side: if matches!(p.side.as_str(), "bid" | "buy") {
                    MakerOrderSide::Buy
                } else {
                    MakerOrderSide::Sell
                },
                amount: fast_float::parse(&p.amount).unwrap_or(0.0),
            })
            .collect();

        mapped.retain(|p| p.amount > 0.0);
        Ok(mapped)
    }

    async fn get_trade_history(
        &self,
        symbol: Option<&str>,
        limit: Option<u32>,
        start_time: Option<u64>,
        end_time: Option<u64>,
    ) -> Result<Vec<MakerTradeHistoryItem>> {
        let rows = self
            .inner
            .get_trade_history(symbol, limit, start_time, end_time)
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| MakerTradeHistoryItem {
                order_id: Some(row.order_id.to_string()),
                client_order_id: row.client_order_id,
                symbol: row.symbol,
                amount: fast_float::parse(&row.amount).unwrap_or(0.0),
                entry_price: fast_float::parse(&row.entry_price).unwrap_or(0.0),
                fee: fast_float::parse(&row.fee).unwrap_or(0.0),
                is_maker_fill: row.event_type == "fulfill_maker",
                created_at_ms: row.created_at,
            })
            .collect())
    }
}

pub struct BinanceMakerExchange {
    inner: Arc<BinanceTrading>,
}

impl BinanceMakerExchange {
    pub fn new(inner: Arc<BinanceTrading>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl MakerExchange for BinanceMakerExchange {
    fn kind(&self) -> MakerExchangeKind {
        MakerExchangeKind::Binance
    }

    fn name(&self) -> &'static str {
        "Binance"
    }

    async fn get_symbol_info(&self, symbol: &str) -> Result<MakerSymbolInfo> {
        self.inner.get_symbol_info(symbol).await
    }

    async fn get_best_bid_ask_rest(&self, symbol: &str, _agg_level: u32) -> Result<Option<(f64, f64)>> {
        self.inner.get_best_bid_ask_rest(symbol).await
    }

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: MakerOrderSide,
        size: f64,
        price: f64,
        _current_bid: Option<f64>,
        _current_ask: Option<f64>,
    ) -> Result<MakerOrderData> {
        self.inner.place_limit_order(symbol, side, size, price).await
    }

    async fn cancel_all_orders(&self, symbol: Option<&str>) -> Result<u32> {
        self.inner.cancel_all_orders(symbol).await
    }

    async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<MakerOpenOrder>> {
        self.inner.get_open_orders(symbol).await
    }

    async fn get_positions(&self, symbol: Option<&str>) -> Result<Vec<MakerPosition>> {
        self.inner.get_positions(symbol).await
    }

    async fn get_trade_history(
        &self,
        symbol: Option<&str>,
        limit: Option<u32>,
        start_time: Option<u64>,
        end_time: Option<u64>,
    ) -> Result<Vec<MakerTradeHistoryItem>> {
        self.inner
            .get_trade_history(symbol, limit, start_time, end_time)
            .await
    }
}
