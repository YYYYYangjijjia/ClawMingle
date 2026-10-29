//! Public TCP egress for Chromium's native SOCKS5 client (RFC 1928 CONNECT).
//! Only destination selection belongs here. TLS, HTTP methods, redirects,
//! cookies, compression, WebSockets and caching remain browser protocols.
use std::{
    collections::HashSet,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, ensure};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy_bidirectional},
    net::{TcpStream, UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

use super::super::social_tools::public_ip;

const CONNECTIONS: usize = 128;
const PENDING: usize = 128;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Policy {
    local_addresses: HashSet<IpAddr>,
}

impl Policy {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            local_addresses: local_addresses()?,
        })
    }

    fn check_addresses(&self, addresses: &[SocketAddr]) -> Result<()> {
        ensure!(
            !addresses.is_empty()
                && addresses.len() <= 32
                && addresses.iter().all(|address| public_ip(address.ip())
                    && !self.local_addresses.contains(&address.ip())),
            "browser_proxy_public_address_required"
        );
        Ok(())
    }

    async fn connect(&self, target: Target) -> Result<TcpStream> {
        let addresses: Vec<_> = match target.host {
            Host::Address(address) => vec![SocketAddr::new(address, target.port)],
            Host::Name(name) => tokio::net::lookup_host((name.as_str(), target.port))
                .await
                .map_err(|_| anyhow::anyhow!("browser_proxy_dns_failed"))?
                .collect(),
        };
        // Reject the whole answer if it contains even one private/local address.
        // Every actual connect uses an already checked numeric address, never
        // a hostname or an environment HTTP proxy that could resolve it again.
        self.check_addresses(&addresses)?;
        for address in addresses {
            if let Ok(Ok(stream)) =
                timeout(Duration::from_secs(5), TcpStream::connect(address)).await
            {
                ensure!(
                    stream.peer_addr()? == address,
                    "browser_proxy_peer_mismatch"
                );
                return Ok(stream);
            }
        }
        anyhow::bail!("browser_proxy_connect_failed")
    }
}

pub(super) async fn serve(listener: UnixListener, cancel: watch::Receiver<bool>, policy: Policy) {
    let policy = Arc::new(policy);
    serve_with_connector(listener, cancel, move |target| {
        let policy = policy.clone();
        async move { policy.connect(target).await }
    })
    .await;
}

async fn serve_with_connector<C, F>(
    listener: UnixListener,
    mut cancel: watch::Receiver<bool>,
    connector: C,
) where
    C: Fn(Target) -> F + Clone + Send + 'static,
    F: Future<Output = Result<TcpStream>> + Send,
{
    let slots = Arc::new(Semaphore::new(CONNECTIONS));
    let mut tasks = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            _ = cancel.changed() => break,
            _ = tasks.join_next(), if !tasks.is_empty() => continue,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else { break };
        if tasks.len() >= CONNECTIONS + PENDING {
            drop(stream);
            continue;
        }
        let slots = slots.clone();
        let connector = connector.clone();
        tasks.spawn(async move {
            let Ok(Ok(_permit)) = timeout(CONNECT_TIMEOUT, slots.acquire_owned()).await else {
                return;
            };
            let _ = proxy_connection(stream, connector).await;
        });
    }
    // Abort drops both ends of every established tunnel, including idle WS/SSE
    // connections. No page/task lifetime or transferred-byte deadline is imposed.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn proxy_connection<C, F>(mut downstream: UnixStream, connector: C) -> Result<()>
where
    C: FnOnce(Target) -> F,
    F: Future<Output = Result<TcpStream>>,
{
    let established = timeout(CONNECT_TIMEOUT, async {
        let target = negotiate(&mut downstream).await?;
        let upstream = match connector(target).await {
            Ok(upstream) => upstream,
            Err(_) => {
                downstream
                    .write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
                    .await?;
                anyhow::bail!("browser_proxy_target_unavailable");
            }
        };
        // Do not disclose host interface addresses in BND.ADDR.
        downstream
            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
            .await?;
        Ok::<_, anyhow::Error>(upstream)
    })
    .await;
    let mut upstream = match established {
        Ok(Ok(upstream)) => upstream,
        _ => return Ok(()),
    };
    copy_bidirectional(&mut downstream, &mut upstream).await?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Host {
    Name(String),
    Address(IpAddr),
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: Host,
    port: u16,
}

async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Target> {
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).await?;
    ensure!(
        greeting[0] == 5 && greeting[1] != 0,
        "browser_proxy_invalid_greeting"
    );
    let mut methods = vec![0; usize::from(greeting[1])];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 255]).await?;
        anyhow::bail!("browser_proxy_authentication_unsupported");
    }
    stream.write_all(&[5, 0]).await?;
    let mut request = [0_u8; 4];
    stream.read_exact(&mut request).await?;
    ensure!(
        request[0] == 5 && request[2] == 0,
        "browser_proxy_invalid_request"
    );
    if request[1] != 1 {
        stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        anyhow::bail!("browser_proxy_connect_required");
    }
    let host = match request[3] {
        1 => {
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).await?;
            Host::Address(Ipv4Addr::from(bytes).into())
        }
        4 => {
            let mut bytes = [0; 16];
            stream.read_exact(&mut bytes).await?;
            Host::Address(Ipv6Addr::from(bytes).into())
        }
        3 => {
            let size = stream.read_u8().await?;
            let mut bytes = vec![0; usize::from(size)];
            stream.read_exact(&mut bytes).await?;
            let name = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow::anyhow!("browser_proxy_invalid_host"))?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if let Ok(address) = name.parse::<IpAddr>() {
                Host::Address(address)
            } else {
                validate_name(&name)?;
                Host::Name(name)
            }
        }
        _ => {
            stream.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            anyhow::bail!("browser_proxy_invalid_address_type");
        }
    };
    let port = stream.read_u16().await?;
    ensure!(port != 0, "browser_proxy_invalid_port");
    Ok(Target { host, port })
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        name.len() <= 253
            && name.contains('.')
            && name.split('.').all(|label| !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
            && ![
                "localhost",
                "local",
                "internal",
                "home.arpa",
                "lan",
                "onion"
            ]
            .iter()
            .any(|suffix| name == *suffix || name.ends_with(&format!(".{suffix}"))),
        "browser_proxy_invalid_host"
    );
    Ok(())
}

/// Read host interface metadata; never change addresses, routes or namespaces.
/// A globally routable address assigned to this host is still a denied target.
fn local_addresses() -> Result<HashSet<IpAddr>> {
    use std::ffi::{c_char, c_int, c_uint, c_void};
    #[repr(C)]
    struct Address {
        family: u16,
        data: [u8; 14],
    }
    #[repr(C)]
    struct AddressV4 {
        family: u16,
        port: u16,
        address: [u8; 4],
        padding: [u8; 8],
    }
    #[repr(C)]
    struct AddressV6 {
        family: u16,
        port: u16,
        flow: u32,
        address: [u8; 16],
        scope: u32,
    }
    #[repr(C)]
    struct Interface {
        next: *mut Interface,
        name: *mut c_char,
        flags: c_uint,
        address: *mut Address,
        netmask: *mut Address,
        broadcast: *mut Address,
        data: *mut c_void,
    }
    unsafe extern "C" {
        fn getifaddrs(addresses: *mut *mut Interface) -> c_int;
        fn freeifaddrs(addresses: *mut Interface);
    }
    struct Interfaces(*mut Interface);
    impl Drop for Interfaces {
        fn drop(&mut self) {
            unsafe { freeifaddrs(self.0) }
        }
    }
    let mut first = std::ptr::null_mut();
    ensure!(
        unsafe { getifaddrs(&mut first) } == 0,
        "browser_proxy_interfaces_unavailable"
    );
    let interfaces = Interfaces(first);
    let mut current = interfaces.0;
    let mut addresses = HashSet::new();
    while !current.is_null() {
        // getifaddrs owns this valid linked list until Interfaces drops it.
        unsafe {
            let address = (*current).address;
            if !address.is_null() {
                match (*address).family {
                    2 => {
                        addresses.insert(IpAddr::V4(Ipv4Addr::from(
                            (*address.cast::<AddressV4>()).address,
                        )));
                    }
                    10 => {
                        addresses.insert(IpAddr::V6(Ipv6Addr::from(
                            (*address.cast::<AddressV6>()).address,
                        )));
                    }
                    _ => {}
                }
            }
            current = (*current).next;
        }
    }
    ensure!(
        !addresses.is_empty(),
        "browser_proxy_interfaces_unavailable"
    );
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn public_targets_exclude_all_local_interfaces_and_mixed_dns_answers() {
        let public = IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4));
        let policy = Policy {
            local_addresses: HashSet::new(),
        };
        assert!(
            policy
                .check_addresses(&[SocketAddr::new(public, 8443)])
                .is_ok()
        );
        assert!(
            policy
                .check_addresses(&[
                    SocketAddr::new(public, 443),
                    SocketAddr::from(([127, 0, 0, 1], 443))
                ])
                .is_err()
        );
        let policy = Policy {
            local_addresses: HashSet::from([public]),
        };
        assert!(
            policy
                .check_addresses(&[SocketAddr::new(public, 443)])
                .is_err()
        );
        for ip in local_addresses().unwrap() {
            assert!(
                Policy::new()
                    .unwrap()
                    .check_addresses(&[SocketAddr::new(ip, 80)])
                    .is_err()
            );
        }
    }

    #[test]
    fn destinations_are_hostnames_not_urls_or_internal_names() {
        assert!(validate_name("example.com").is_ok());
        for name in [
            "",
            "localhost",
            "example.local",
            "metadata.internal",
            "http://example.com",
            "a@b.example",
            "a..example",
            "-a.example",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }

    #[tokio::test]
    async fn native_socks_handshake_handles_fragmented_domains_and_rejects_udp() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let parsed = tokio::spawn(async move { negotiate(&mut server).await });
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0]);
        for byte in b"\x05\x01\x00\x03\x0bexample.com\x20\xfb" {
            client.write_u8(*byte).await.unwrap();
            tokio::task::yield_now().await;
        }
        assert_eq!(
            parsed.await.unwrap().unwrap(),
            Target {
                host: Host::Name("example.com".into()),
                port: 8443
            }
        );

        let (mut client, mut server) = tokio::io::duplex(1024);
        let parsed = tokio::spawn(async move { negotiate(&mut server).await });
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.read_exact(&mut reply).await.unwrap();
        client.write_all(&[5, 3, 0, 1]).await.unwrap();
        let mut rejected = [0; 10];
        client.read_exact(&mut rejected).await.unwrap();
        assert_eq!(rejected[1], 7);
        assert!(parsed.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn cancellation_closes_both_sides_of_an_established_native_tunnel() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("proxy.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = target.local_addr().unwrap();
        let (cancel, cancelled) = watch::channel(false);
        // Only this test connector maps documentation-domain traffic to its
        // synthetic listener. Production always uses Policy::connect above.
        let task = tokio::spawn(serve_with_connector(
            listener,
            cancelled,
            move |request| async move {
                assert_eq!(
                    request,
                    Target {
                        host: Host::Name("example.com".into()),
                        port: 80
                    }
                );
                Ok(TcpStream::connect(address).await?)
            },
        ));
        let mut client = UnixStream::connect(&socket).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0; 2];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        client
            .write_all(b"\x05\x01\x00\x03\x0bexample.com\x00\x50")
            .await
            .unwrap();
        let (mut remote, _) = target.accept().await.unwrap();
        let mut response = [0; 10];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(response[1], 0);
        client.write_all(b"post body").await.unwrap();
        let mut body = [0; 9];
        remote.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"post body");
        remote.write_all(b"WS").await.unwrap();
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"WS");
        cancel.send(true).unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), client.read(&mut greeting))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            timeout(Duration::from_secs(2), remote.read(&mut greeting))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
