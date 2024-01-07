#![forbid(unsafe_code)]
use log::{debug, error, info, trace, warn};

use anyhow::anyhow;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;
use url::{Host, ParseError, Url};

use crate::types::{KittyProxyError, ResponseCode};
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

#[derive(Clone)]
pub struct HttpProxy {
    listener: Arc<Mutex<TcpListener>>,
    // Timeout for connections
    timeout: Option<Duration>,
    shutdown_flag: Arc<AtomicBool>,
    vpn_host: String,
    vpn_port: u16,
}

impl HttpProxy {
    /// Create a new Merino instance
    pub async fn new(
        ip: &str,
        port: u16,
        timeout: Option<Duration>,
        vpn_host: &str,
        vpn_port: u16,
    ) -> io::Result<Self> {
        info!("Listening on {}:{}", ip, port);
        let listener = TcpListener::bind((ip, port)).await?;
        let listener = Arc::new(Mutex::new(listener));
        Ok(Self {
            listener,
            timeout,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            vpn_host: vpn_host.to_string(),
            vpn_port,
        })
    }

    pub async fn get_local_addr(&self) -> SocketAddr {
        self.listener.lock().await.local_addr().unwrap()
    }

    pub async fn serve(&mut self, match_proxy: Arc<MatchProxy>) {
        info!("Serving Connections...");

        while let Ok((stream, client_addr)) = self.listener.lock().await.accept().await {
            println!("serve incoming!!!");
            if self.shutdown_flag.load(Ordering::Relaxed) {
                println!("shutdown");
                break;
            }
            let timeout = self.timeout.clone();
            let vpn_host = self.vpn_host.clone();
            let vpn_port = self.vpn_port.clone();
            let match_proxy_clone = Arc::clone(&match_proxy);
            tokio::spawn(async move {
                let mut client = HttpClient::new(stream, timeout);
                match client
                    .handle_client(match_proxy_clone.as_ref(), vpn_host.as_str(), vpn_port)
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        error!("Error! {:?}, client: {:?}", error, client_addr);

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
    }

    pub async fn quit(&self, local_addr: &str) {
        println!("quit called");
        self.shutdown_flag.store(true, Ordering::Relaxed);
        // let local_addr = self.listener.lock().await.local_addr();
        println!("local_addr: {:?}", local_addr);
        let res = TcpStream::connect(local_addr).await;
        match res {
            Ok(_) => {}
            Err(error) => {
                println!("error: {}", error)
            }
        }
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
        match_proxy: &MatchProxy,
        vpn_host: &str,
        vpn_port: u16,
    ) -> Result<usize, KittyProxyError> {
        debug!("Starting to relay data");
        let req: HttpReq = HttpReq::from_stream(&mut self.stream).await?;
        let time_out = if let Some(time_out) = self.timeout {
            time_out
        } else {
            Duration::from_millis(500)
        };

        let match_res = match_proxy.traffic_stream(&req.host);
        let target_server = if match_res {
            trace!("direct connect");
            format!("{}:{}", req.host, req.port)
        } else {
            trace!("proxy connect");
            format!("{vpn_host}:{vpn_port}")
        };
        trace!("req.target_server: {}", target_server);
        let mut target_stream =
            timeout(
                time_out,
                async move { TcpStream::connect(target_server).await },
            )
            .await
            .map_err(|_| KittyProxyError::Proxy(ResponseCode::ConnectionRefused))??;

        if req.method == "CONNECT" && match_res {
            self.stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
        } else {
            target_stream.write_all(&req.readed_buffer).await?;
        }

        trace!("copy bidirectional");
        match tokio::io::copy_bidirectional(&mut self.stream, &mut target_stream).await {
            // ignore not connected for shutdown error
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {
                trace!("already closed");
                Ok(0)
            }
            Err(e) => Err(KittyProxyError::Io(e)),
            Ok((_s_to_t, t_to_s)) => Ok(t_to_s as usize),
        }
    }
}

/// Proxy User Request
#[allow(dead_code)]
struct HttpReq {
    pub method: String,
    pub host: Host,
    pub port: u16,
    pub readed_buffer: Vec<u8>,
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
        let request_first_line = request_headers.get(0).unwrap().clone();
        let mut parts = request_first_line.split_whitespace();
        let method = parts.next().expect("Invalid request");
        let origin_path = parts.next().expect("Invalid request");
        let version = parts.next().expect("Invalid request");
        trace!("http req path:{origin_path}, method:{method}, version:{version}");

        if version != "HTTP/1.1" {
            warn!("Init: Unsupported version: HTTP{}", version);
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
        trace!("host: {:?}", host);
        Ok(HttpReq {
            method: method.to_string(),
            host,
            port,
            readed_buffer: request_headers.join("").as_bytes().to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Ok;

    use super::*;
    use std::path::PathBuf;
    use std::str::FromStr;

    #[tokio::test]
    async fn it_works() -> anyhow::Result<()> {
        let geoip_file = "/home/hezhaozhao/opensource/kitty/src-tauri/binaries/geoip.dat";
        let geosite_file = "/home/hezhaozhao/opensource/kitty/src-tauri/binaries/geosite.dat";
        let match_proxy = MatchProxy::from_geo_dat(
            Some(&PathBuf::from_str(geoip_file).unwrap()),
            Some(&PathBuf::from_str(geosite_file).unwrap()),
        )
        .unwrap();
        let arc_match_proxy = Arc::new(match_proxy);
        let mut proxy = HttpProxy::new("127.0.0.1", 10088, None, "127.0.0.1", 10809).await?;
        let proxy_clone = proxy.clone();
        let arc_match_proxy_clone = arc_match_proxy.clone();
        trace!("aaaaa");
        tokio::spawn(async move {
            let _ = proxy.serve(arc_match_proxy_clone).await;
        });
        let local_addr = proxy_clone.get_local_addr().await;
        println!("{:?}", local_addr);
        trace!("call quit before");

        tokio::time::sleep(Duration::from_secs(20)).await;
        // let _ = proxy.serve(arc_match_proxy_clone).await;
        proxy_clone.quit(local_addr.to_string().as_str()).await;

        tokio::time::sleep(Duration::from_secs(20)).await;
        Ok(())
    }
}
