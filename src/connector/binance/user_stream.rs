use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use tokio::time::{interval, sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, warn};

use super::trading::BinanceCredentials;
use crate::connector::maker::MakerOrderSide;

const MAINNET_REST_URL: &str = "https://fapi.binance.com";
const TESTNET_REST_URL: &str = "https://testnet.binancefuture.com";
const MAINNET_WS_BASE_URL: &str = "wss://fstream.binance.com/ws";
const TESTNET_WS_BASE_URL: &str = "wss://stream.binancefuture.com/ws";

#[derive(Debug, Clone)]
pub struct BinanceUserStreamConfig {
    pub reconnect_attempts: u32,
    pub ping_interval_secs: u64,
    pub listen_key_keepalive_secs: u64,
}

impl Default for BinanceUserStreamConfig {
    fn default() -> Self {
        Self {
            reconnect_attempts: 10,
            ping_interval_secs: 20,
            listen_key_keepalive_secs: 30 * 60,
        }
    }
}

#[derive(Debug, Clone)]
pub enum BinanceUserStreamEvent {
    Trade {
        symbol: String,
        side: MakerOrderSide,
        client_order_id: Option<String>,
        order_id: Option<String>,
        last_fill_qty: f64,
        cumulative_fill_qty: f64,
        fill_price: f64,
        order_status: String,
        trade_time_ms: u64,
    },
    Cancelled {
        symbol: String,
        client_order_id: Option<String>,
        order_id: Option<String>,
    },
}

#[derive(Clone)]
pub struct BinanceUserStreamClient {
    credentials: BinanceCredentials,
    config: BinanceUserStreamConfig,
    rest_url: String,
    ws_base_url: String,
    client: Client,
}

impl BinanceUserStreamClient {
    pub fn new(
        credentials: BinanceCredentials,
        is_testnet: bool,
        config: BinanceUserStreamConfig,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("Failed to build Binance user stream HTTP client")?;

        Ok(Self {
            credentials,
            config,
            rest_url: if is_testnet {
                TESTNET_REST_URL.to_string()
            } else {
                MAINNET_REST_URL.to_string()
            },
            ws_base_url: if is_testnet {
                TESTNET_WS_BASE_URL.to_string()
            } else {
                MAINNET_WS_BASE_URL.to_string()
            },
            client,
        })
    }

    pub async fn start<F>(&self, mut callback: F) -> Result<()>
    where
        F: FnMut(BinanceUserStreamEvent) + Send + 'static,
    {
        let mut reconnect_count = 0u32;

        loop {
            match self.connect_and_run(&mut callback).await {
                Ok(()) => {
                    info!("[BINANCE_USER_STREAM] Stream closed gracefully");
                    return Ok(());
                }
                Err(e) => {
                    reconnect_count += 1;
                    error!(
                        "[BINANCE_USER_STREAM] Stream error (attempt {}/{}): {}",
                        reconnect_count, self.config.reconnect_attempts, e
                    );

                    if reconnect_count >= self.config.reconnect_attempts {
                        return Err(anyhow!(
                            "[BINANCE_USER_STREAM] Failed after {} reconnect attempts",
                            self.config.reconnect_attempts
                        ));
                    }

                    let backoff = if reconnect_count == 1 {
                        1
                    } else {
                        std::cmp::min(2u64.pow(reconnect_count - 1), 30)
                    };
                    warn!("[BINANCE_USER_STREAM] Reconnecting in {}s...", backoff);
                    sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
    }

    async fn create_listen_key(&self) -> Result<String> {
        #[derive(Debug, Deserialize)]
        struct ListenKeyResponse {
            #[serde(rename = "listenKey")]
            listen_key: String,
        }

        let url = format!("{}/fapi/v1/listenKey", self.rest_url);
        let response = self
            .client
            .post(&url)
            .header("X-MBX-APIKEY", &self.credentials.api_key)
            .send()
            .await
            .context("Failed to create Binance listenKey")?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "Failed to create Binance listenKey: {} - {}",
                status,
                text
            ));
        }

        let parsed: ListenKeyResponse = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse listenKey response: {text}"))?;
        Ok(parsed.listen_key)
    }

    async fn keepalive_listen_key(&self, listen_key: &str) -> Result<()> {
        let url = format!("{}/fapi/v1/listenKey?listenKey={}", self.rest_url, listen_key);
        let response = self
            .client
            .put(&url)
            .header("X-MBX-APIKEY", &self.credentials.api_key)
            .send()
            .await
            .context("Failed to keepalive Binance listenKey")?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Binance listenKey keepalive failed: {} - {}",
                status,
                text
            ));
        }

        Ok(())
    }

    async fn close_listen_key(&self, listen_key: &str) {
        let url = format!("{}/fapi/v1/listenKey?listenKey={}", self.rest_url, listen_key);
        let _ = self
            .client
            .delete(&url)
            .header("X-MBX-APIKEY", &self.credentials.api_key)
            .send()
            .await;
    }

    async fn connect_and_run<F>(&self, callback: &mut F) -> Result<()>
    where
        F: FnMut(BinanceUserStreamEvent) + Send + 'static,
    {
        let listen_key = self.create_listen_key().await?;
        info!(
            "[BINANCE_USER_STREAM] listenKey created ({}...{})",
            &listen_key.chars().take(8).collect::<String>(),
            &listen_key
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );

        let ws_url = format!("{}/{}", self.ws_base_url, listen_key);
        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .with_context(|| format!("Failed to connect user stream WS: {ws_url}"))?;
        info!("[BINANCE_USER_STREAM] WebSocket connected");

        let (mut write, mut read) = ws_stream.split();
        let mut ping_interval = interval(Duration::from_secs(self.config.ping_interval_secs));
        ping_interval.tick().await;

        let keepalive_client = self.clone();
        let keepalive_listen_key = listen_key.clone();
        let keepalive_interval_secs = self.config.listen_key_keepalive_secs.max(60);
        let keepalive_task = tokio::spawn(async move {
            let mut keepalive_interval = interval(Duration::from_secs(keepalive_interval_secs));
            keepalive_interval.tick().await;
            loop {
                keepalive_interval.tick().await;
                if let Err(e) = keepalive_client.keepalive_listen_key(&keepalive_listen_key).await {
                    warn!("[BINANCE_USER_STREAM] listenKey keepalive failed: {}", e);
                } else {
                    debug!("[BINANCE_USER_STREAM] listenKey keepalive sent");
                }
            }
        });

        let stream_result: Result<()> = async {
            loop {
                tokio::select! {
                    _ = ping_interval.tick() => {
                        write.send(Message::Ping(Vec::new())).await
                            .context("Failed to send user stream ping")?;
                    }
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                self.handle_message(&text, callback)?;
                            }
                            Some(Ok(Message::Ping(data))) => {
                                write.send(Message::Pong(data)).await
                                    .context("Failed to reply pong")?;
                            }
                            Some(Ok(Message::Pong(_))) => {}
                            Some(Ok(Message::Close(frame))) => {
                                warn!("[BINANCE_USER_STREAM] WS closed by server: {:?}", frame);
                                return Err(anyhow!("User stream WS closed by server"));
                            }
                            Some(Err(e)) => {
                                return Err(anyhow!("User stream WS error: {}", e));
                            }
                            None => {
                                return Err(anyhow!("User stream WS ended"));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        .await;

        keepalive_task.abort();
        self.close_listen_key(&listen_key).await;
        stream_result
    }

    fn handle_message<F>(&self, text: &str, callback: &mut F) -> Result<()>
    where
        F: FnMut(BinanceUserStreamEvent),
    {
        #[derive(Debug, Deserialize)]
        struct StreamEnvelope {
            #[serde(rename = "e")]
            event_type: Option<String>,
            #[serde(rename = "o")]
            order: Option<OrderUpdateData>,
        }

        #[derive(Debug, Deserialize)]
        struct OrderUpdateData {
            #[serde(rename = "s")]
            symbol: String,
            #[serde(rename = "S")]
            side: String,
            #[serde(rename = "X")]
            order_status: String,
            #[serde(rename = "x")]
            execution_type: String,
            #[serde(rename = "i")]
            order_id: i64,
            #[serde(rename = "c")]
            client_order_id: String,
            #[serde(rename = "l")]
            last_fill_qty: String,
            #[serde(rename = "z")]
            cumulative_fill_qty: String,
            #[serde(rename = "L")]
            last_fill_price: String,
            #[serde(rename = "ap")]
            average_price: String,
            #[serde(rename = "T")]
            trade_time: u64,
        }

        let msg: StreamEnvelope = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                debug!("[BINANCE_USER_STREAM] Ignoring non-json message: {}", e);
                return Ok(());
            }
        };

        if let Some(event_type) = msg.event_type.as_deref() {
            if event_type == "listenKeyExpired" {
                return Err(anyhow!("Binance listenKey expired"));
            }
        }

        let order = match msg.order {
            Some(v) => v,
            None => return Ok(()),
        };

        if order.execution_type == "TRADE" {
            let last_fill_qty = order.last_fill_qty.parse::<f64>().unwrap_or(0.0);
            let cumulative_fill_qty = order.cumulative_fill_qty.parse::<f64>().unwrap_or(0.0);
            let last_fill_price = order.last_fill_price.parse::<f64>().unwrap_or(0.0);
            let average_price = order.average_price.parse::<f64>().unwrap_or(0.0);
            let fill_price = if last_fill_price > 0.0 {
                last_fill_price
            } else {
                average_price
            };
            if last_fill_qty <= 0.0 {
                return Ok(());
            }

            let side = if order.side.eq_ignore_ascii_case("BUY") {
                MakerOrderSide::Buy
            } else {
                MakerOrderSide::Sell
            };

            callback(BinanceUserStreamEvent::Trade {
                symbol: order.symbol,
                side,
                client_order_id: Some(order.client_order_id),
                order_id: Some(order.order_id.to_string()),
                last_fill_qty,
                cumulative_fill_qty,
                fill_price,
                order_status: order.order_status,
                trade_time_ms: order.trade_time,
            });
        } else if order.order_status == "CANCELED" || order.execution_type == "CANCELED" {
            callback(BinanceUserStreamEvent::Cancelled {
                symbol: order.symbol,
                client_order_id: Some(order.client_order_id),
                order_id: Some(order.order_id.to_string()),
            });
        }

        Ok(())
    }
}
