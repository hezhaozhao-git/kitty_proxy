#![forbid(unsafe_code)]
use log::{debug, error, info, trace, warn};

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
#[cfg(windows)]
use tokio::signal::windows::ctrl_c;
use tokio::time::timeout;

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

pub struct HttpProxy {
    listener: TcpListener,
    // Timeout for connections
    timeout: Option<Duration>,
    shutdown_flag: AtomicBool,
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
        Ok(Self {
            listener: TcpListener::bind((ip, port)).await?,
            timeout,
            shutdown_flag: AtomicBool::new(false),
            vpn_host: vpn_host.to_string(),
            vpn_port,
        })
    }

    pub async fn serve(&mut self, match_proxy: Arc<MatchProxy>) {
        info!("Serving Connections...");

        while let Ok((stream, client_addr)) = self.listener.accept().await {
            let timeout = self.timeout.clone();
            let vpn_host = self.vpn_host.clone();
            let vpn_port = self.vpn_port.clone();
            let match_proxy_clone = Arc::clone(&match_proxy);
            tokio::spawn(async move {
                let mut client = HttpClient::new(stream, timeout);
                match client
                    .init(match_proxy_clone, vpn_host.as_str(), vpn_port)
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

    async fn quit(&self) {
        #[cfg(unix)]
        {
            let mut term = signal(SignalKind::terminate())
                .expect("Failed to register terminate signal handler");
            let mut interrupt = signal(SignalKind::interrupt())
                .expect("Failed to register interrupt signal handler");

            tokio::select! {
                _ = term.recv() => {
                    println!("Received terminate signal");
                }
                _ = interrupt.recv() => {
                    println!("Received interrupt signal");
                }
            }

            self.shutdown_flag.store(true, Ordering::Relaxed);
        }

        #[cfg(windows)]
        {
            let _ = ctrl_c().await;
            println!("Received Ctrl+C signal");

            self.shutdown_flag.store(true, Ordering::Relaxed);
        }
    }
}

pub struct HttpClient<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> {
    stream: T,
    http_version: String,
    timeout: Option<Duration>,
}

impl<T> HttpClient<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    /// Create a new SOCKClient
    pub fn new(stream: T, timeout: Option<Duration>) -> Self {
        Self {
            stream,
            http_version: String::from("HTTP/1.1"),
            timeout,
        }
    }

    /// Shutdown a client
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }

    pub async fn init(
        &mut self,
        match_proxy: Arc<MatchProxy>,
        vpn_host: &str,
        vpn_port: u16,
    ) -> Result<(), KittyProxyError> {
        debug!("New connection");
        let mut header = [0u8; 2];
        // Read a byte from the stream and determine the version being requested
        self.stream.read_exact(&mut header).await?;

        trace!("Version: {}", self.http_version,);

        match self.http_version.as_str() {
            "HTTP/1.1" => {
                // Authenticate w/ client
                self.handle_client(match_proxy.as_ref(), vpn_host, vpn_port)
                    .await?;
            }
            _ => {
                warn!("Init: Unsupported version: HTTP{}", self.http_version);
                self.shutdown().await?;
            }
        }

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
        let req = HttpReq::from_stream(&mut self.stream).await?;
        let time_out = if let Some(time_out) = self.timeout {
            time_out
        } else {
            Duration::from_millis(500)
        };
        trace!("req.target_server: {}", req.target_server);
        let match_res = match_proxy.match_cn_domain(req.target_server.as_str());
        let mut target_stream = if match_res {
            trace!("direct connect");
            timeout(time_out, async move {
                TcpStream::connect(req.target_server).await
            })
            .await
            .map_err(|_| KittyProxyError::Proxy(ResponseCode::ConnectionRefused))??
        } else {
            trace!("proxy connect");
            timeout(time_out, async move {
                TcpStream::connect(format!("{vpn_host}:{vpn_port}")).await
            })
            .await
            .map_err(|_| KittyProxyError::Proxy(ResponseCode::ConnectionRefused))??
        };

        trace!("copy bidirectional");
        target_stream.write_all(&req.readed_buffer).await?;
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
    pub target_server: String,
    pub readed_buffer: Vec<u8>,
}

impl HttpReq {
    /// Parse a SOCKS Req from a TcpStream
    async fn from_stream<T>(stream: &mut T) -> Result<Self, KittyProxyError>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let mut reader = BufReader::new(stream);
        let mut request_first_line = String::new();
        let _ = reader.read_line(&mut request_first_line).await?;
        let mut parts = request_first_line.split_whitespace();
        let _method = parts.next().expect("Invalid request");
        let path = parts.next().expect("Invalid request");
        let _version = parts.next().expect("Invalid request");
        Ok(HttpReq {
            target_server: path.to_string(),
            readed_buffer: (request_first_line.clone() + "\n").as_bytes().to_vec(),
        })
    }
}
