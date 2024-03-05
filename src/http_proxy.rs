#![forbid(unsafe_code)]

use log::{debug, error, info, warn};

use anyhow::anyhow;
use anyhow::Result;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch::Receiver;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::time::timeout;
use url::{Host, ParseError, Url};

use crate::traffic_diversion::TrafficStreamRule;
use crate::types::{KittyProxyError, NodeInfo, NodeStatistics, ResponseCode, StatisticsMap};
use crate::MatchProxy;

pub struct HttpReply {
    buf: Vec<u8>,
}

impl HttpReply {
    pub fn new(status: ResponseCode) -> Self {
        let mut buffer: Vec<u8> = Vec::new();
        let response = format!(
            "HTTP/1.1 {} Proxy Error\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\
             \r\n\
             Proxy Error",
            status as usize, 11
        );

        buffer.extend_from_slice(response.as_bytes());
        Self { buf: buffer }
    }

    pub async fn send<T>(&self, stream: &mut T) -> io::Result<()>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        stream.write_all(&self.buf[..]).await?;
        Ok(())
    }
}

pub struct HttpProxy {
    ip: String,
    port: u16,
    timeout: Option<Duration>,
    node_statistics_map: StatisticsMap,
    is_serve: bool,
}

impl HttpProxy {
    pub async fn new(ip: &str, port: u16, timeout: Option<Duration>) -> io::Result<Self> {
        info!("Http proxy listening on {}:{}", ip, port);
        Ok(Self {
            ip: ip.to_string(),
            port,
            timeout,
            node_statistics_map: Arc::new(Mutex::new(None)),
            is_serve: false,
        })
    }

    pub async fn serve(
        &mut self,
        match_proxy: Arc<RwLock<MatchProxy>>,
        rx: &mut Receiver<bool>,
        vpn_node_infos: Vec<NodeInfo>,
    ) {
        let listener = TcpListener::bind((self.ip.clone(), self.port))
            .await
            .unwrap();
        self.is_serve = true;
        let timeout = self.timeout.clone();
        let match_proxy_clone = Arc::clone(&match_proxy);
        let mut rx_clone = rx.clone();
        let mut statistics_map = self.node_statistics_map.lock().await;
        *statistics_map = Some(NodeStatistics::from_vec(&vpn_node_infos));
        drop(statistics_map);
        let statistics_map_clone = Arc::clone(&self.node_statistics_map);
        tokio::spawn(async move {
            tokio::select! {
                _ = async {
                    loop {
                        let (stream, client_addr) = listener.accept().await.unwrap();
                        let match_proxy_clone = match_proxy_clone.clone();
                        let statistics_map_clone = statistics_map_clone.clone();
                        tokio::spawn(async move {
                            let mut client = HttpClient::new(stream, timeout);
                match client
                    .handle_client(match_proxy_clone, statistics_map_clone)
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        debug!("Error {:?}, client: {:?}", error, client_addr);
                        if let Err(e) = HttpReply::new(error.into()).send(&mut client.stream).await
                        {
                            warn!("Failed to send error code: {:?}", e);
                        }
                        if let Err(e) = client.shutdown().await {
                            warn!("Failed to shutdown TcpStream: {:?}", e);
                        };
                    }
                };

                         });
                    }
                } => {}
                _ =  async {
                        if rx_clone.changed().await.is_ok() {
                            return//该任务退出，别的也会停
                    }
                } => {}
            }
        });
    }

    pub fn is_serving(&self) -> bool {
        self.is_serve
    }
}

pub struct HttpClient<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> {
    stream: T,
    timeout: Option<Duration>,
}

impl<T> HttpClient<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    /// Create a new SOCKClient
    pub fn new(stream: T, timeout: Option<Duration>) -> Self {
        Self { stream, timeout }
    }

    /// Shutdown a client
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }

    /// Handles a client
    pub async fn handle_client(
        &mut self,
        match_proxy_share: Arc<RwLock<MatchProxy>>,
        vpn_node_statistics_map: StatisticsMap,
    ) -> Result<usize, KittyProxyError> {
        let req: HttpReq = HttpReq::from_stream(&mut self.stream).await?;
        let time_out = if let Some(time_out) = self.timeout {
            time_out
        } else {
            Duration::from_millis(1000)
        };
        let match_proxy = match_proxy_share.read().await;
        let rule = match_proxy.traffic_stream(&req.host);
        drop(match_proxy);
        info!("HTTP [TCP] {}:{} {} connect", req.host, req.port, rule);

        let is_direct = match rule {
            TrafficStreamRule::Reject => {
                self.shutdown().await?;
                return Ok(0 as usize);
            }
            TrafficStreamRule::Direct => true,
            TrafficStreamRule::Proxy => false,
        };
        let node_info = if !is_direct {
            let vpn_node_statistics = vpn_node_statistics_map.lock().await;
            let vpn_node_statistics_ref = vpn_node_statistics.as_ref().unwrap();
            Some(vpn_node_statistics_ref.get_least_connected_node().await)
        } else {
            None
        };
        let target_server = if is_direct {
            format!("{}:{}", req.host, req.port)
        } else {
            node_info.unwrap().socket_addr.to_string()
        };
        debug!("target_server: {}", target_server);
        let mut target_stream =
            timeout(
                time_out,
                async move { TcpStream::connect(target_server).await },
            )
            .await
            .map_err(|_|{
                error!("HTTP error {}:{} connect timeout", req.host, req.port);
                KittyProxyError::Proxy(ResponseCode::ConnectionRefused)
            } )??;
        if !is_direct {
            let mut vpn_node_statistics = vpn_node_statistics_map.lock().await;
            let vpn_node_statistics = vpn_node_statistics.as_mut().unwrap();
            vpn_node_statistics.incre_count_by_node_info(&node_info.unwrap());
        }

        if req.method == "CONNECT" && is_direct {
            self.stream
                .write_all(format!("{} 200 Connection established\r\n\r\n", req.version).as_bytes())
                .await?;
        } else {
            target_stream.write_all(&req.readed_buffer).await?;
        }

        let return_value =
            match tokio::io::copy_bidirectional(&mut self.stream, &mut target_stream).await {
                // ignore not connected for shutdown error
                Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {
                    error!("HTTP error {}:{} {}", req.host, req.port, e);
                    Ok(0)
                }
                Err(e) => {
                    error!("HTTP error {}:{} {}", req.host, req.port, e);
                    Err(KittyProxyError::Io(e))
                },
                Ok((_s_to_t, t_to_s)) => Ok(t_to_s as usize),
            };
        if !is_direct {
            let mut vpn_node_statistics = vpn_node_statistics_map.lock().await;
            let vpn_node_statistics = vpn_node_statistics.as_mut().unwrap();
            vpn_node_statistics.decre_count_by_node_info(&node_info.unwrap());
        }
        return_value
    }
}

/// Proxy User Request
#[allow(dead_code)]
struct HttpReq {
    pub method: String,
    pub host: Host,
    pub port: u16,
    pub readed_buffer: Vec<u8>,
    pub version: String,
}

impl HttpReq {
    /// Parse a SOCKS Req from a TcpStream
    async fn from_stream<T>(stream: &mut T) -> Result<Self, KittyProxyError>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let mut request_headers: Vec<String> = Vec::new();
        let mut reader: BufReader<&mut T> = BufReader::new(stream);

        loop {
            let mut tmp = String::new();
            reader.read_line(&mut tmp).await?;
            request_headers.push(tmp.clone());
            if tmp == "\r\n" {
                break;
            }
        }
        println!("request_headers: {:?}", request_headers);
        let request_first_line = request_headers.get(0).unwrap().clone();
        let mut parts = request_first_line.split_whitespace();
        let method = parts.next().expect("Invalid request");
        let origin_path = parts.next().expect("Invalid request");
        let version = parts.next().expect("Invalid request");
        debug!("http req path:{origin_path}, method:{method}, version:{version}");

        if version != "HTTP/1.1" && version != "HTTP/1.0" {
            debug!("Init: Unsupported version: {}", version);
            stream.shutdown().await?;
            return Err(anyhow!(format!("Not support version: {}.", version)).into());
        }

        let mut origin_path = origin_path.to_string();
        if method == "CONNECT" {
            origin_path.insert_str(0, "http://")
        };
        let url = Url::parse(&origin_path)?;
        let host = url.host().map(|x| x.to_owned());
        let port = url.port().unwrap_or(80);
        let host = host.ok_or(ParseError::EmptyHost)?;
        Ok(HttpReq {
            method: method.to_string(),
            host,
            port,
            readed_buffer: request_headers.join("").as_bytes().to_vec(),
            version: version.into(),
        })
    }
}
