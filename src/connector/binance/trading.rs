use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::connector::maker::{
    MakerOpenOrder, MakerOrderData, MakerOrderSide, MakerPosition, MakerSymbolInfo,
    MakerTradeHistoryItem,
};

type HmacSha256 = Hmac<Sha256>;

const MAINNET_REST_URL: &str = "https://fapi.binance.com";
const TESTNET_REST_URL: &str = "https://testnet.binancefuture.com";

#[derive(Debug, Clone)]
pub struct BinanceCredentials {
    pub api_key: String,
    pub api_secret: String,
}

impl BinanceCredentials {
    pub fn from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        let api_key = std::env::var("BINANCE_API_KEY")
            .context("BINANCE_API_KEY not found in environment")?;
        let api_secret = std::env::var("BINANCE_API_SECRET")
            .context("BINANCE_API_SECRET not found in environment")?;

        Ok(Self {
            api_key,
            api_secret,
        })
    }
}

pub struct BinanceTrading {
    credentials: BinanceCredentials,
    rest_url: String,
    client: Client,
    symbol_info_cache: Arc<RwLock<HashMap<String, MakerSymbolInfo>>>,
}

impl BinanceTrading {
    pub fn new(credentials: BinanceCredentials, is_testnet: bool) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to build Binance HTTP client")?;

        Ok(Self {
            credentials,
            rest_url: if is_testnet {
                TESTNET_REST_URL.to_string()
            } else {
                MAINNET_REST_URL.to_string()
            },
            client,
            symbol_info_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    fn normalize_symbol(symbol: &str) -> String {
        let upper = symbol.to_uppercase();
        if upper.ends_with("USDT")
            || upper.ends_with("BUSD")
            || upper.ends_with("USDC")
            || upper.ends_with("FDUSD")
        {
            upper
        } else {
            format!("{upper}USDT")
        }
    }

    fn decimal_to_string(v: f64) -> String {
        let mut s = format!("{:.12}", v);
        while s.contains('.') && s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    }

    fn round_down_by_step(value: f64, step: f64) -> f64 {
        if step <= 0.0 {
            return value;
        }
        (value / step).floor() * step
    }

    fn sign_query(&self, query: &str) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(self.credentials.api_secret.as_bytes())
            .context("Failed to initialize Binance HMAC signer")?;
        mac.update(query.as_bytes());
        let result = mac.finalize().into_bytes();
        Ok(hex::encode(result))
    }

    async fn signed_request_json(
        &self,
        method: Method,
        path: &str,
        mut params: Vec<(String, String)>,
    ) -> Result<serde_json::Value> {
        params.push(("timestamp".to_string(), Utc::now().timestamp_millis().to_string()));
        params.push(("recvWindow".to_string(), "5000".to_string()));

        let query = serde_urlencoded::to_string(&params).context("Failed to encode Binance query")?;
        let signature = self.sign_query(&query)?;
        let url = format!("{base}{path}?{query}&signature={signature}", base = self.rest_url);

        let response = self
            .client
            .request(method, &url)
            .header("X-MBX-APIKEY", &self.credentials.api_key)
            .send()
            .await
            .context("Binance signed request failed")?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Binance signed request failed: {} - {}", status, text);
        }

        serde_json::from_str(&text).with_context(|| format!("Failed to parse Binance JSON: {text}"))
    }

    async fn signed_request_parse<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        params: Vec<(String, String)>,
    ) -> Result<T> {
        let json = self.signed_request_json(method, path, params).await?;
        serde_json::from_value(json).context("Failed to deserialize Binance signed response")
    }

    async fn unsigned_request_parse<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: Vec<(String, String)>,
    ) -> Result<T> {
        let query = serde_urlencoded::to_string(&params).context("Failed to encode Binance query")?;
        let sep = if query.is_empty() { "" } else { "?" };
        let url = format!("{}{}{}{}", self.rest_url, path, sep, query);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Binance unsigned request failed")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Binance unsigned request failed: {} - {}", status, text);
        }

        serde_json::from_str::<T>(&text)
            .with_context(|| format!("Failed to parse Binance response: {text}"))
    }

    async fn fetch_exchange_info(&self, symbol: &str) -> Result<MakerSymbolInfo> {
        #[derive(Debug, Deserialize)]
        struct ExchangeInfoResponse {
            symbols: Vec<ExchangeSymbol>,
        }

        #[derive(Debug, Deserialize)]
        struct ExchangeSymbol {
            symbol: String,
            filters: Vec<ExchangeFilter>,
        }

        #[derive(Debug, Deserialize)]
        struct ExchangeFilter {
            #[serde(rename = "filterType")]
            filter_type: String,
            #[serde(rename = "tickSize")]
            tick_size: Option<String>,
            #[serde(rename = "stepSize")]
            step_size: Option<String>,
        }

        let exchange_info: ExchangeInfoResponse = self
            .unsigned_request_parse(
                "/fapi/v1/exchangeInfo",
                vec![("symbol".to_string(), Self::normalize_symbol(symbol))],
            )
            .await?;

        let target_symbol = Self::normalize_symbol(symbol);
        let s = exchange_info
            .symbols
            .into_iter()
            .find(|x| x.symbol == target_symbol)
            .with_context(|| format!("Binance symbol not found: {target_symbol}"))?;

        let tick_size = s
            .filters
            .iter()
            .find(|f| f.filter_type == "PRICE_FILTER")
            .and_then(|f| f.tick_size.as_deref())
            .context("Missing PRICE_FILTER.tickSize")?
            .parse::<f64>()
            .context("Failed to parse tickSize")?;

        let lot_size = s
            .filters
            .iter()
            .find(|f| f.filter_type == "LOT_SIZE")
            .and_then(|f| f.step_size.as_deref())
            .context("Missing LOT_SIZE.stepSize")?
            .parse::<f64>()
            .context("Failed to parse stepSize")?;

        Ok(MakerSymbolInfo {
            tick_size,
            lot_size,
        })
    }

    pub async fn get_symbol_info(&self, symbol: &str) -> Result<MakerSymbolInfo> {
        let key = Self::normalize_symbol(symbol);
        if let Some(cached) = self.symbol_info_cache.read().await.get(&key).cloned() {
            return Ok(cached);
        }

        let info = self.fetch_exchange_info(symbol).await?;
        self.symbol_info_cache.write().await.insert(key, info.clone());
        Ok(info)
    }

    pub async fn get_best_bid_ask_rest(&self, symbol: &str) -> Result<Option<(f64, f64)>> {
        #[derive(Debug, Deserialize)]
        struct BookTicker {
            #[serde(rename = "bidPrice")]
            bid_price: String,
            #[serde(rename = "askPrice")]
            ask_price: String,
        }

        let data: BookTicker = self
            .unsigned_request_parse(
                "/fapi/v1/ticker/bookTicker",
                vec![("symbol".to_string(), Self::normalize_symbol(symbol))],
            )
            .await?;

        let bid = data.bid_price.parse::<f64>().unwrap_or(0.0);
        let ask = data.ask_price.parse::<f64>().unwrap_or(0.0);
        if bid <= 0.0 || ask <= 0.0 {
            return Ok(None);
        }

        Ok(Some((bid, ask)))
    }

    pub async fn place_limit_order(
        &self,
        symbol: &str,
        side: MakerOrderSide,
        size: f64,
        price: f64,
    ) -> Result<MakerOrderData> {
        #[derive(Debug, Deserialize)]
        struct NewOrderResponse {
            #[serde(rename = "orderId")]
            order_id: i64,
            #[serde(rename = "clientOrderId")]
            client_order_id: String,
            symbol: String,
        }

        let symbol_info = self.get_symbol_info(symbol).await?;
        let rounded_qty = Self::round_down_by_step(size, symbol_info.lot_size);
        let rounded_px = Self::round_down_by_step(price, symbol_info.tick_size);
        if rounded_qty <= 0.0 || rounded_px <= 0.0 {
            bail!(
                "Rounded Binance order values are invalid: qty={}, price={}",
                rounded_qty,
                rounded_px
            );
        }

        let client_id = format!("xemm-{}", Uuid::new_v4().simple());
        let side_str = match side {
            MakerOrderSide::Buy => "BUY",
            MakerOrderSide::Sell => "SELL",
        };

        let order: NewOrderResponse = self
            .signed_request_parse(
                Method::POST,
                "/fapi/v1/order",
                vec![
                    ("symbol".to_string(), Self::normalize_symbol(symbol)),
                    ("side".to_string(), side_str.to_string()),
                    ("type".to_string(), "LIMIT".to_string()),
                    ("timeInForce".to_string(), "GTX".to_string()), // post-only
                    ("quantity".to_string(), Self::decimal_to_string(rounded_qty)),
                    ("price".to_string(), Self::decimal_to_string(rounded_px)),
                    ("newClientOrderId".to_string(), client_id),
                ],
            )
            .await?;

        Ok(MakerOrderData {
            order_id: Some(order.order_id.to_string()),
            client_order_id: Some(order.client_order_id),
            symbol: Some(order.symbol),
        })
    }

    pub async fn place_market_order(
        &self,
        symbol: &str,
        side: MakerOrderSide,
        size: f64,
    ) -> Result<MakerOrderData> {
        #[derive(Debug, Deserialize)]
        struct NewOrderResponse {
            #[serde(rename = "orderId")]
            order_id: i64,
            #[serde(rename = "clientOrderId")]
            client_order_id: String,
            symbol: String,
        }

        let symbol_info = self.get_symbol_info(symbol).await?;
        let rounded_qty = Self::round_down_by_step(size, symbol_info.lot_size);
        if rounded_qty <= 0.0 {
            bail!("Rounded Binance market qty is invalid: {}", rounded_qty);
        }

        let client_id = format!("xemm-mkt-{}", Uuid::new_v4().simple());
        let side_str = match side {
            MakerOrderSide::Buy => "BUY",
            MakerOrderSide::Sell => "SELL",
        };

        let order: NewOrderResponse = self
            .signed_request_parse(
                Method::POST,
                "/fapi/v1/order",
                vec![
                    ("symbol".to_string(), Self::normalize_symbol(symbol)),
                    ("side".to_string(), side_str.to_string()),
                    ("type".to_string(), "MARKET".to_string()),
                    ("quantity".to_string(), Self::decimal_to_string(rounded_qty)),
                    ("newClientOrderId".to_string(), client_id),
                ],
            )
            .await?;

        Ok(MakerOrderData {
            order_id: Some(order.order_id.to_string()),
            client_order_id: Some(order.client_order_id),
            symbol: Some(order.symbol),
        })
    }

    pub async fn cancel_all_orders(&self, symbol: Option<&str>) -> Result<u32> {
        let symbol = symbol.context("Binance cancel_all_orders requires symbol")?;
        let before_count = self.get_open_orders(Some(symbol)).await.map(|v| v.len()).unwrap_or(0);

        let _ = self
            .signed_request_json(
                Method::DELETE,
                "/fapi/v1/allOpenOrders",
                vec![("symbol".to_string(), Self::normalize_symbol(symbol))],
            )
            .await?;

        let after_count = self.get_open_orders(Some(symbol)).await.map(|v| v.len()).unwrap_or(0);
        Ok(before_count.saturating_sub(after_count) as u32)
    }

    pub async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<MakerOpenOrder>> {
        #[derive(Debug, Deserialize)]
        struct OpenOrderRow {
            symbol: String,
            side: String,
            price: String,
            #[serde(rename = "origQty")]
            orig_qty: String,
            #[serde(rename = "executedQty")]
            executed_qty: String,
            #[serde(rename = "orderId")]
            order_id: i64,
            #[serde(rename = "clientOrderId")]
            client_order_id: String,
        }

        let mut params = Vec::new();
        if let Some(sym) = symbol {
            params.push(("symbol".to_string(), Self::normalize_symbol(sym)));
        }

        let rows: Vec<OpenOrderRow> = self
            .signed_request_parse(Method::GET, "/fapi/v1/openOrders", params)
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| MakerOpenOrder {
                order_id: Some(row.order_id.to_string()),
                client_order_id: row.client_order_id,
                symbol: row.symbol,
                side: if row.side.eq_ignore_ascii_case("BUY") {
                    MakerOrderSide::Buy
                } else {
                    MakerOrderSide::Sell
                },
                price: row.price.parse::<f64>().unwrap_or(0.0),
                initial_amount: row.orig_qty.parse::<f64>().unwrap_or(0.0),
                filled_amount: row.executed_qty.parse::<f64>().unwrap_or(0.0),
            })
            .collect())
    }

    pub async fn get_positions(&self, symbol: Option<&str>) -> Result<Vec<MakerPosition>> {
        #[derive(Debug, Deserialize)]
        struct PositionRow {
            symbol: String,
            #[serde(rename = "positionAmt")]
            position_amt: String,
        }

        let rows: Vec<PositionRow> = self
            .signed_request_parse(Method::GET, "/fapi/v2/positionRisk", vec![])
            .await?;

        let symbol_filter = symbol.map(Self::normalize_symbol);
        let mut out = Vec::new();
        for row in rows {
            if let Some(ref sf) = symbol_filter {
                if &row.symbol != sf {
                    continue;
                }
            }
            let signed = row.position_amt.parse::<f64>().unwrap_or(0.0);
            if signed.abs() < 1e-12 {
                continue;
            }
            out.push(MakerPosition {
                symbol: row.symbol,
                side: if signed > 0.0 {
                    MakerOrderSide::Buy
                } else {
                    MakerOrderSide::Sell
                },
                amount: signed.abs(),
            });
        }

        Ok(out)
    }

    pub async fn get_trade_history(
        &self,
        symbol: Option<&str>,
        limit: Option<u32>,
        start_time: Option<u64>,
        end_time: Option<u64>,
    ) -> Result<Vec<MakerTradeHistoryItem>> {
        #[derive(Debug, Deserialize)]
        struct UserTradeRow {
            symbol: String,
            #[serde(rename = "orderId")]
            order_id: i64,
            price: String,
            qty: String,
            commission: String,
            maker: bool,
            time: u64,
        }

        let symbol = symbol.context("Binance trade history requires symbol")?;
        let mut params = vec![
            ("symbol".to_string(), Self::normalize_symbol(symbol)),
            (
                "limit".to_string(),
                limit.unwrap_or(50).min(1000).to_string(),
            ),
        ];
        if let Some(v) = start_time {
            params.push(("startTime".to_string(), v.to_string()));
        }
        if let Some(v) = end_time {
            params.push(("endTime".to_string(), v.to_string()));
        }

        let rows: Vec<UserTradeRow> = self
            .signed_request_parse(Method::GET, "/fapi/v1/userTrades", params)
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| MakerTradeHistoryItem {
                order_id: Some(row.order_id.to_string()),
                client_order_id: None,
                symbol: row.symbol,
                amount: row.qty.parse::<f64>().unwrap_or(0.0),
                entry_price: row.price.parse::<f64>().unwrap_or(0.0),
                fee: row.commission.parse::<f64>().unwrap_or(0.0),
                is_maker_fill: row.maker,
                created_at_ms: row.time,
            })
            .collect())
    }
}
