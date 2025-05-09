use std::collections::HashMap;

use futures::{prelude::*, stream::SplitStream, StreamExt};
use pin_project::*;
use reqwest::header;
use std::time::Duration;
use streamunordered::{StreamUnordered, StreamYield};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
use url::Url;

use failure;
use serde_json;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use crate::client::Kucoin;
use crate::error::APIError;
use crate::model::websocket::{
    DefaultMsg, InstanceServers, KucoinWebsocketMsg, Subscribe, WSTopic, WSType,
};
use crate::model::{APIDatum, Method};
use crate::utils::get_time;

type WSStream = WebSocketStream<
    tokio_tungstenite::stream::Stream<TcpStream, tokio_native_tls::TlsStream<TcpStream>>,
>;
pub type StoredStream = SplitStream<WSStream>;

#[pin_project]
#[derive(Default)]
pub struct KucoinWebsocket {
    subscriptions: HashMap<WSTopic, usize>,
    tokens: HashMap<usize, WSTopic>,
    #[pin]
    streams: StreamUnordered<StoredStream>,
}

impl Stream for KucoinWebsocket {
    type Item = Result<KucoinWebsocketMsg, APIError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.as_mut().project().streams.poll_next(cx) {
            Poll::Ready(Some((y, _))) => match y {
                StreamYield::Item(item) => {
                    // let heartbeat = self.heartbeats.get_mut(&token).unwrap();
                    Poll::Ready(Some(
                        item.map_err(APIError::Websocket).and_then(parse_message),
                    ))
                }
                StreamYield::Finished(_) => Poll::Pending,
            },
            Poll::Ready(None) => panic!("No Stream Subscribed"),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl KucoinWebsocket {
    pub async fn subscribe(&mut self, url: String, ws_topic: Vec<WSTopic>) -> Result<(), APIError> {
        let endpoint = Url::parse(&url);
        if endpoint.is_err() {
            return Err(APIError::Other("invalid url".to_string()));
        }
        let endpoint = endpoint.unwrap();
        //println!("Kucoin endpoint: {}", endpoint);

        let (ws_stream, _) = connect_async(endpoint).await?;

        let (sink, read) = ws_stream.split();
        let sink_mutex = Mutex::new(sink);

        for topic in ws_topic.iter() {
            let sub = Subscribe::new(topic);
            //println!("Subscribing to topic: {}", sub.topic);

            sink_mutex
                .lock()
                .await
                .send(Message::Text(serde_json::to_string(&sub).unwrap()))
                .await?;
        }

        // Ping heartbeat
        tokio::spawn(async move {
            loop {
                time::sleep(Duration::from_secs(30)).await;
                let ping = DefaultMsg {
                    id: get_time().to_string(),
                    r#type: "ping".to_string(),
                };
                let resp = sink_mutex
                    .lock()
                    .await
                    .send(Message::Text(serde_json::to_string(&ping).unwrap()))
                    .map_err(APIError::Websocket)
                    .await;

                if let Err(e) = resp {
                    match e {
                        APIError::Websocket(e) => {
                            format_err!("Error sending Ping: {}", e);
                            break;
                        }
                        _ => format_err!("None websocket error sending Ping: {}", e),
                    };
                };
            }
        });

        let token = self.streams.insert(read);
        self.subscriptions.insert(ws_topic[0].clone(), token);
        self.tokens.insert(token, ws_topic[0].clone());

        Ok(())
    }

    pub fn unsubscribe(&mut self, ws_topic: WSTopic) -> Option<StoredStream> {
        let streams = Pin::new(&mut self.streams);
        self.subscriptions
            .get(&ws_topic)
            .and_then(|token| StreamUnordered::take(streams, *token))
    }
}

fn parse_message(msg: Message) -> Result<KucoinWebsocketMsg, APIError> {
    match msg {
        Message::Text(msg) => {
            if msg.contains("\"type\":\"welcome\"") || msg.contains("\"type\":\"ack\"") {
                Ok(KucoinWebsocketMsg::WelcomeMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("\"type\":\"ping\"") {
                Ok(KucoinWebsocketMsg::PingMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("\"type\":\"pong\"") {
                Ok(KucoinWebsocketMsg::PongMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("\"subject\":\"trade.ticker\"") {
                Ok(KucoinWebsocketMsg::TickerMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("\"topic\":\"/market/ticker:all\"") {
                Ok(KucoinWebsocketMsg::AllTickerMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"trade.snapshot\"") {
                Ok(KucoinWebsocketMsg::SnapshotMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("\"subject\":\"trade.l2update\"") {
                Ok(KucoinWebsocketMsg::OrderBookMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("/market/match:") {
                Ok(KucoinWebsocketMsg::MatchMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("\"subject\":\"trade.l3received\"") {
                Ok(KucoinWebsocketMsg::Level3ReceivedMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"trade.l3open\"") {
                Ok(KucoinWebsocketMsg::Level3OpenMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"trade.l3done\"") {
                Ok(KucoinWebsocketMsg::Level3DoneMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"trade.l3match\"") {
                Ok(KucoinWebsocketMsg::Level3MatchMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"trade.l3change\"") {
                Ok(KucoinWebsocketMsg::Level3ChangeMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"level2\"") {
                Ok(KucoinWebsocketMsg::OrderBookDepthMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"received\"") {
                Ok(KucoinWebsocketMsg::FullMatchReceivedMsg(
                    serde_json::from_str(&msg)?,
                ))
            } else if msg.contains("\"subject\":\"open\"") {
                Ok(KucoinWebsocketMsg::FullMatchOpenMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"done\"") {
                Ok(KucoinWebsocketMsg::FullMatchDoneMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"match\"") {
                Ok(KucoinWebsocketMsg::FullMatchMatchMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("\"subject\":\"update\"") {
                Ok(KucoinWebsocketMsg::FullMatchChangeMsg(
                    serde_json::from_str(&msg)?,
                ))
            } else if msg.contains("/indicator/index:") {
                Ok(KucoinWebsocketMsg::IndexPriceMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("/indicator/markPrice:") {
                Ok(KucoinWebsocketMsg::MarketPriceMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("/margin/fundingBook:") {
                Ok(KucoinWebsocketMsg::OrderBookChangeMsg(
                    serde_json::from_str(&msg)?,
                ))
            } else if msg.contains("\"type\":\"stop\"") || msg.contains("\"type\":\"activate\"") {
                Ok(KucoinWebsocketMsg::StopOrderMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("/account/balance") {
                Ok(KucoinWebsocketMsg::BalancesMsg(serde_json::from_str(&msg)?))
            } else if msg.contains("debt.ratio") {
                Ok(KucoinWebsocketMsg::DebtRatioMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("position.status") {
                Ok(KucoinWebsocketMsg::PositionChangeMsg(serde_json::from_str(
                    &msg,
                )?))
            } else if msg.contains("order.open") {
                Ok(KucoinWebsocketMsg::MarginTradeOpenMsg(
                    serde_json::from_str(&msg)?,
                ))
            } else if msg.contains("order.update") {
                Ok(KucoinWebsocketMsg::MarginTradeUpdateMsg(
                    serde_json::from_str(&msg)?,
                ))
            } else if msg.contains("order.done") {
                Ok(KucoinWebsocketMsg::MarginTradeDoneMsg(
                    serde_json::from_str(&msg)?,
                ))
            } else if msg.contains("error") {
                Ok(KucoinWebsocketMsg::Error(msg))
            } else if msg.contains("\"topic\":\"/spotMarket/tradeOrders") {
                // Supports both TradeOrders and TradeOrdersV2
                if msg.contains("\"type\":\"received\"") {
                    Ok(KucoinWebsocketMsg::TradeReceivedMsg(
                        serde_json::from_str(&msg).expect("TradeReceivedMsg serde fail"),
                    ))
                } else if msg.contains("\"type\":\"open\"") {
                    Ok(KucoinWebsocketMsg::TradeOpenMsg(
                        serde_json::from_str(&msg).expect("TradeOpenMsg serde fail"),
                    ))
                } else if msg.contains("\"type\":\"match\"") {
                    Ok(KucoinWebsocketMsg::TradeMatchMsg(
                        serde_json::from_str(&msg).expect("TradeMatchMsg serde fail"),
                    ))
                } else if msg.contains("\"type\":\"filled\"") {
                    Ok(KucoinWebsocketMsg::TradeFilledMsg(
                        serde_json::from_str(&msg).expect("TradeFilledMsg serde fail"),
                    ))
                } else if msg.contains("\"type\":\"canceled\"") {
                    Ok(KucoinWebsocketMsg::TradeCanceledMsg(
                        serde_json::from_str(&msg).expect("TradeCanceledMsg serde fail"),
                    ))
                } else if msg.contains("\"type\":\"update\"") {
                    Ok(KucoinWebsocketMsg::TradeUpdateMsg(
                        serde_json::from_str(&msg).expect("TradeUpdateMsg serde fail"),
                    ))
                } else {
                    Err(APIError::Other(
                        "Unrecognised message from tradeOrders\n".to_string()
                            + &serde_json::to_string_pretty(&msg).unwrap(),
                    ))
                }
            } else if msg.contains("\"topic\":\"/contractMarket/ticker") {
                if msg.contains("\"subject\":\"ticker\"") {
                    Ok(KucoinWebsocketMsg::FutTickerMsg(serde_json::from_str(&msg)?))
                }
                else if msg.contains("\"subject\":\"snapshot\"") {
                    Ok(KucoinWebsocketMsg::SnapshotMsg(serde_json::from_str(&msg)?))
                } else if msg.contains("\"subject\":\"l2update\"") {
                    Ok(KucoinWebsocketMsg::OrderBookMsg(serde_json::from_str(&msg)?))
                } else if msg.contains("\"subject\":\"l3received\"") {
                    Ok(KucoinWebsocketMsg::Level3ReceivedMsg(
                        serde_json::from_str(&msg)?,
                    ))
                } else if msg.contains("\"subject\":\"l3open\"") {
                    Ok(KucoinWebsocketMsg::Level3OpenMsg(serde_json::from_str(&msg)?))
                } else if msg.contains("\"subject\":\"l3done\"") {
                    Ok(KucoinWebsocketMsg::Level3DoneMsg(serde_json::from_str(&msg)?))
                } else if msg.contains("\"subject\":\"l3match\"") {
                    Ok(KucoinWebsocketMsg::Level3MatchMsg(serde_json::from_str(&msg)?))
                } else if msg.contains("\"subject\":\"l3change\"") {
                    Ok(KucoinWebsocketMsg::Level3ChangeMsg(serde_json::from_str(&msg)?))
                } else {
                    Err(APIError::Other(
                        "Unrecognised message from contractMarket\n".to_string()
                            + &serde_json::to_string_pretty(&msg).unwrap(),
                    ))
                }
            } else {
                Err(APIError::Other(
                    format!("No KucoinWebSocketMsg type to parse:\n{}", msg)
                ))
            }
        }
        Message::Binary(b) => Ok(KucoinWebsocketMsg::Binary(b)),
        Message::Pong(..) => Ok(KucoinWebsocketMsg::Pong),
        Message::Ping(..) => Ok(KucoinWebsocketMsg::Ping),
        Message::Close(..) => Err(APIError::Other("Socket closed error".to_string())),
    }
}

pub async fn close_socket(
    heartbeat: &mut tokio::task::JoinHandle<()>,
) -> Result<(), failure::Error> {
    heartbeat.await?;
    Ok(())
}

impl Kucoin {
    pub fn websocket(&self) -> KucoinWebsocket {
        KucoinWebsocket::default()
    }

    pub async fn ws_bullet_private(&self) -> Result<APIDatum<InstanceServers>, APIError> {
        let endpoint = String::from("/api/v1/bullet-private");

        let url: String = format!("{}{}", &self.prefix, endpoint);
        let header: header::HeaderMap = self
            .sign_headers(endpoint, None, None, Method::POST)
            .unwrap();
        let resp = self.post(url, Some(header), None).await?;
        let api_data: APIDatum<InstanceServers> = resp.json().await?;
        // println!("ws_bullet_private api:\n{api_data:#?}");
        Ok(api_data)
    }

    pub async fn ws_bullet_public(&self) -> Result<APIDatum<InstanceServers>, APIError> {
        let endpoint = String::from("/api/v1/bullet-public");
        let url: String = format!("{}{}", &self.prefix, endpoint);
        let header: header::HeaderMap = self
            .sign_headers(endpoint, None, None, Method::POST)
            .unwrap();
        let resp = self.post(url, Some(header), None).await?;
        let api_data: APIDatum<InstanceServers> = resp.json().await?;
        Ok(api_data)
    }

    pub async fn get_socket_endpoint(&self, ws_type: WSType) -> Result<String, APIError> {
        let endpoint: String;
        let token: String;
        let timestamp = get_time();
        match ws_type {
            WSType::Private => {
                let resp = self.ws_bullet_private().await?;
                if let Some(r) = resp.data {
                    token = r.token.to_owned();
                    endpoint = r.instance_servers[0].endpoint.to_owned();
                } else {
                    let message = resp.msg.unwrap_or_else(|| "no data or message".to_string());
                    return Err(APIError::Other(message));
                }
            }
            WSType::Public => {
                let resp = self.ws_bullet_public().await?;
                if let Some(r) = &resp.data {
                    token = r.token.to_owned();
                    endpoint = r.instance_servers[0].endpoint.to_owned();
                } else {
                    let message = resp.msg.unwrap_or_else(|| "no data or message".to_string());
                    return Err(APIError::Other(message));
                }
            }
        }
        if endpoint.is_empty() || token.is_empty() {
            return Err(APIError::Other("Missing endpoint/token".to_string()));
        }
        let url = format!(
            "{}?token={}&[connectId={}]?acceptUserMessage=\"true\"",
            endpoint, token, timestamp
        );
        Ok(url)
    }
}

impl Subscribe {
    pub fn new(topic_type: &WSTopic) -> Self {
        let id = get_time().to_string();
        let mut private_channel = false;
        let topic = match topic_type {
            WSTopic::Ticker(ref symbols) => format!("/market/ticker:{}", symbols.join(",")),
            WSTopic::FutTicker(ref symbols) => format!("/contractMarket/ticker:{}", symbols.join(",")),
            WSTopic::AllTicker => String::from("/market/ticker:all"),
            WSTopic::Snapshot(ref symbol) => format!("/market/snapshot:{}", symbol),
            WSTopic::OrderBook(ref symbols) => format!("/market/level2:{}", symbols.join(",")),
            WSTopic::OrderBookDepth5(ref symbols) => {
                format!("/spotMarket/level2Depth5:{}", symbols.join(","))
            }
            WSTopic::OrderBookDepth50(ref symbols) => {
                format!("/spotMarket/level2Depth50:{}", symbols.join(","))
            }
            WSTopic::Match(ref symbols) => format!("/market/match:{}", symbols.join(",")),
            WSTopic::FullMatch(ref symbols) => format!("/spotMarket/level3:{}", symbols.join(",")),
            WSTopic::Level3Public(ref symbols) => format!("/market/level3:{}", symbols.join(",")),
            WSTopic::Level3Private(ref symbols) => {
                private_channel = true;
                format!("/market/level3:{}", symbols.join(","))
            }
            WSTopic::IndexPrice(ref symbols) => format!("/indicator/index:{}", symbols.join(",")),
            WSTopic::MarketPrice(ref symbols) => {
                format!("/indicator/markPrice:{}", symbols.join(","))
            }
            WSTopic::OrderBookChange(ref symbols) => {
                format!("/margin/fundingBook:{}", symbols.join(","))
            }
            WSTopic::StopOrder(ref symbols) => {
                private_channel = true;
                format!("/market/level3:{}", symbols.join(","))
            }
            WSTopic::Balances => {
                private_channel = true;
                String::from("/account/balance")
            }
            WSTopic::DebtRatio => {
                private_channel = true;
                String::from("/margin/position")
            }
            WSTopic::PositionChange => {
                private_channel = true;
                String::from("/margin/position")
            }
            WSTopic::MarginTradeOrder(ref symbol) => {
                private_channel = true;
                format!("/margin/loan:{}", symbol)
            }
            WSTopic::TradeOrders => {
                private_channel = true;
                String::from("/spotMarket/tradeOrders")
            }
            WSTopic::TradeOrdersV2 => {
                private_channel = true;
                String::from("/spotMarket/tradeOrdersV2")
            }
        };

        Subscribe {
            id,
            r#type: String::from("subscribe"),
            topic,
            private_channel,
            response: true,
        }
    }
}
