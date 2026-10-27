use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ironrdp_cfg::PropertySetExt as _;
use ironrdp_client::avd::{
    AvdGatewayChallenge, AvdHostPopTokenRequest, AvdOAuthMode, AvdTokenProvider, ClaimsChallenge,
    OAuthAvdTokenProvider, decode_redirected_auth_blob, decode_redirected_server_cert,
    extract_ip_from_network_metadata, normalize_oauth_authority, parse_ice_servers_config,
};
use ironrdp_client::config::ConfigBuilder;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::server_redirection::{ELEMENT_TYPE_CERTIFICATE, ENCODING_TYPE_ASN1_DER};
use ironrdp_propertyset::PropertySet;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use uuid::Uuid;

const DEFAULT_WVD_CLIENT_ID: &str = "a85cf173-4192-42f8-81fa-777a763e6e2c";

// oracle: reference MSRDC AVD WWW-Authenticate Claims challenge format
#[test]
fn parse_claims_challenge_header() {
    let header = "Claims:Authority=https://login.windows.net/4c5a5ec1-9ac5-4612-9b4c-6bed178bb65a;Client=a85cf173-4192-42f8-81fa-777a763e6e2c;Redirect=https://www.wvd.microsoft.com/webclient;Resource=https://www.wvd.microsoft.com;Site=501413";
    let challenge = ClaimsChallenge::parse(header).unwrap();
    assert_eq!(
        challenge.authority.as_deref(),
        Some("https://login.windows.net/4c5a5ec1-9ac5-4612-9b4c-6bed178bb65a")
    );
    assert_eq!(
        challenge.client_id.as_deref(),
        Some("a85cf173-4192-42f8-81fa-777a763e6e2c")
    );
    assert_eq!(challenge.resource.as_deref(), Some("https://www.wvd.microsoft.com"));
    assert_eq!(challenge.site_id.as_deref(), Some("501413"));
}

// oracle: spec MS-RDPBCGR §2.2.13.1.2; property round-trip
#[test]
fn decode_redirected_cert_and_auth_blob_roundtrip() {
    let cert_der = b"\x30\x82\x01\x0a\x02\x82\x01\x01\x00\xab\xcd\xef";
    let mut container = Vec::new();
    container.extend_from_slice(&ELEMENT_TYPE_CERTIFICATE.to_le_bytes());
    container.extend_from_slice(&ENCODING_TYPE_ASN1_DER.to_le_bytes());
    container.extend_from_slice(&(u32::try_from(cert_der.len()).unwrap()).to_le_bytes());
    container.extend_from_slice(cert_der);
    let inner_b64 = STANDARD.encode(&container);
    let mut utf16_bytes = Vec::new();
    for u in inner_b64.encode_utf16() {
        utf16_bytes.extend_from_slice(&u.to_le_bytes());
    }
    utf16_bytes.extend_from_slice(&[0, 0]);
    let outer_b64 = STANDARD.encode(&utf16_bytes);

    assert_eq!(decode_redirected_server_cert(&outer_b64).unwrap(), cert_der);

    let cookie = b"broker-auth-cookie-bytes";
    let inner_cookie_b64 = STANDARD.encode(cookie);
    let mut cookie_utf16 = Vec::new();
    for u in inner_cookie_b64.encode_utf16() {
        cookie_utf16.extend_from_slice(&u.to_le_bytes());
    }
    let outer_cookie_b64 = STANDARD.encode(&cookie_utf16);
    assert_eq!(decode_redirected_auth_blob(&outer_cookie_b64).unwrap(), cookie);
}

// oracle: reference MSRDC AVD orchestration iceServersConfig and azureInstanceNetworkMetadata JSON
#[test]
fn parse_ice_servers_and_shortpath_network_metadata() {
    let ice_json = serde_json::json!({
        "iceServers": [
            {
                "urls": ["stun:world.turn.wvd.microsoft.com:3478", "turn:world.turn.wvd.microsoft.com:3478?transport=udp"],
                "username": "turn-user",
                "credential": "turn-secret"
            }
        ]
    });
    let servers = parse_ice_servers_config(&ice_json);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].urls.len(), 2);
    assert_eq!(servers[0].username.as_deref(), Some("turn-user"));
    assert_eq!(servers[0].credential.as_deref(), Some("turn-secret"));

    let net_meta = serde_json::json!({ "publicIpAddress": "20.50.100.10" });
    assert_eq!(
        extract_ip_from_network_metadata(&net_meta),
        Some("20.50.100.10".parse().unwrap())
    );
}

struct MockAvdTokenProvider;

impl AvdTokenProvider for MockAvdTokenProvider {
    fn acquire_gateway_token(&self, challenge: &AvdGatewayChallenge) -> anyhow::Result<(String, String)> {
        assert_eq!(challenge.resource, "https://www.wvd.microsoft.com");
        Ok(("mock-gw-token".to_owned(), "user1@downhillpro.xyz".to_owned()))
    }

    fn acquire_host_pop_token(&self, request: &AvdHostPopTokenRequest) -> anyhow::Result<String> {
        assert!(request.resource_uri.starts_with("ms-device-service://"));
        assert!(!request.req_cnf.is_empty());
        Ok("mock-pop-token".to_owned())
    }
}

// oracle: spec crates/ironrdp-client/README.md §IronRDP client
#[test]
fn custom_avd_token_provider_roundtrip() {
    let provider: Arc<dyn AvdTokenProvider> = Arc::new(MockAvdTokenProvider);
    let (gw_tok, user) = provider
        .acquire_gateway_token(&AvdGatewayChallenge {
            authority: "https://login.windows.net/common".to_owned(),
            client_id: DEFAULT_WVD_CLIENT_ID.to_owned(),
            resource: "https://www.wvd.microsoft.com".to_owned(),
            site_id: Some("501413".to_owned()),
            username_hint: None,
        })
        .unwrap();
    assert_eq!(gw_tok, "mock-gw-token");
    assert_eq!(user, "user1@downhillpro.xyz");
}

// oracle: spec RFC 6749 §6
#[test]
fn oauth_authority_allowlist_and_cross_authority_cache_lookup() {
    assert_eq!(
        normalize_oauth_authority("https://login.windows.net/4c5a5ec1-9ac5-4612-9b4c-6bed178bb65a/").unwrap(),
        "https://login.microsoftonline.com/4c5a5ec1-9ac5-4612-9b4c-6bed178bb65a"
    );
    assert!(normalize_oauth_authority("https://login.microsoftonline.com.evil.example/common").is_err());
    assert!(normalize_oauth_authority("http://login.microsoftonline.com/common").is_err());

    let provider = OAuthAvdTokenProvider::with_cache_path(AvdOAuthMode::Auto, None).unwrap();
    provider.store_refresh_token_for_test(
        "https://login.microsoftonline.com/common",
        DEFAULT_WVD_CLIENT_ID,
        "rt-cross-tenant",
        "user1@downhillpro.xyz",
    );
    let (found_rt, found_user) = provider
        .lookup_cached_refresh_token_for_test(
            "https://login.microsoftonline.com/4c5a5ec1-9ac5-4612-9b4c-6bed178bb65a",
            DEFAULT_WVD_CLIENT_ID,
            Some("user1@downhillpro.xyz"),
        )
        .unwrap();
    assert_eq!(found_rt, "rt-cross-tenant");
    assert_eq!(found_user, "user1@downhillpro.xyz");

    assert!(
        provider
            .lookup_cached_refresh_token_for_test(
                "https://login.microsoftonline.com/common",
                DEFAULT_WVD_CLIENT_ID,
                Some("other@downhillpro.xyz"),
            )
            .is_none(),
        "unmatched username hint must not return another account's refresh token"
    );
}

// oracle: spec crates/ironrdp-client/README.md §IronRDP client
#[test]
fn avd_gateway_config_accepts_logon_credentials_source_and_redacts_token_properties() {
    let mut ps = PropertySet::new();
    ps.insert("full address", "sessionhost.wvd.microsoft.com:3389");
    ps.insert("gatewayhostname", "afdfp-rdgateway-r1.wvd.microsoft.com:443");
    ps.insert("gatewayusagemethod", 1i64);
    ps.insert("gatewaycredentialssource", 5i64);
    ps.insert("gatewaybrokeringtype", 1i64);
    ps.insert("enablerdsaadauth", 1i64);
    ps.insert("ironrdp_avd_gateway_token", "secret-gw-token");
    ps.insert("ironrdp_avd_sso_logon_token", "secret-sso-token");

    let cfg = ConfigBuilder::from_property_set(&ps)
        .unwrap()
        .with_client_build(1)
        .with_client_dir("C:\\")
        .with_client_name("client")
        .with_platform(MajorPlatformType::WINDOWS)
        .build()
        .unwrap();
    assert_eq!(cfg.certificate_validation(), ironrdp_tls::CertificateValidation::Strict);
    assert!(cfg.properties().get::<&str>("ironrdp_avd_gateway_token").is_none());
    assert!(cfg.properties().get::<&str>("ironrdp_avd_sso_logon_token").is_none());

    let mut direct_entra = PropertySet::new();
    direct_entra.insert("full address", "sessionhost.wvd.microsoft.com:3389");
    direct_entra.insert("enablerdsaadauth", 1i64);
    assert!(
        ConfigBuilder::from_property_set(&direct_entra)
            .unwrap()
            .with_client_build(1)
            .with_client_dir("C:\\")
            .with_client_name("client")
            .with_platform(MajorPlatformType::WINDOWS)
            .build()
            .is_err(),
        "property-only enablerdsaadauth without an AVD gateway or EntraAuthSession must be rejected"
    );
}

// oracle: reference Windows App companion <rdp_file>.json UserName metadata
#[test]
fn rdp_launch_companion_reads_added_json_extension() {
    let base = std::env::temp_dir().join(format!("ironrdp-companion-{}.rdp", Uuid::new_v4()));
    let mut added_os = base.as_os_str().to_os_string();
    added_os.push(".json");
    let companion_path = std::path::PathBuf::from(added_os);
    std::fs::write(&companion_path, r#"{"UserName":"user1@downhillpro.xyz"}"#).unwrap();

    let mut ps = PropertySet::new();
    ConfigBuilder::apply_rdp_launch_companion(&base, &mut ps);
    let _ = std::fs::remove_file(&companion_path);

    assert_eq!(ps.username(), Some("user1@downhillpro.xyz"));
}

// oracle: spec RFC 8628 §3.2; spec RFC 6749 §6
#[tokio::test]
async fn oauth_device_code_refresh_and_pop_exchange_roundtrip() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let id_payload = URL_SAFE_NO_PAD.encode(r#"{"preferred_username":"user1@downhillpro.xyz"}"#);
    let fake_id_token = ["e30", id_payload.as_str(), "c2ln"].join(".");

    let server = tokio::spawn(async move {
        for step in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::with_capacity(4096);
            let mut chunk = [0u8; 1024];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(0 < n, "unexpected EOF reading OAuth test HTTP request headers");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let header_str = String::from_utf8_lossy(&buf[..header_end]);
            let content_len = header_str
                .lines()
                .find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buf.len() < header_end + content_len {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(0 < n, "unexpected EOF reading OAuth test HTTP request body");
                buf.extend_from_slice(&chunk[..n]);
            }
            let req = String::from_utf8_lossy(&buf);

            let body_json = match step {
                0 => {
                    assert!(req.starts_with("POST /tenant/oauth2/v2.0/devicecode "));
                    serde_json::json!({
                        "device_code": "dev-code-123",
                        "user_code": "ABCD-EFGH",
                        "verification_uri": "https://login.microsoft.com/device",
                        "expires_in": 900,
                        "interval": 1,
                        "message": "Enter ABCD-EFGH at https://login.microsoft.com/device"
                    })
                    .to_string()
                }
                1 => {
                    assert!(req.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
                    serde_json::json!({
                        "access_token": "gw-access-token-1",
                        "refresh_token": "refresh-token-1",
                        "id_token": fake_id_token,
                    })
                    .to_string()
                }
                2 => {
                    assert!(req.contains("grant_type=refresh_token"));
                    assert!(req.contains("refresh_token=refresh-token-1"));
                    assert!(req.contains("req_cnf=kid-123"));
                    assert!(req.contains("token_type=pop"));
                    serde_json::json!({
                        "access_token": "host-pop-token-1",
                        "refresh_token": "refresh-token-2",
                    })
                    .to_string()
                }
                _ => {
                    assert!(req.contains("grant_type=refresh_token"));
                    assert!(req.contains("refresh_token=refresh-token-2"));
                    serde_json::json!({
                        "access_token": "gw-access-token-2",
                        "refresh_token": "refresh-token-2",
                    })
                    .to_string()
                }
            };

            let http_resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_json}",
                body_json.len()
            );
            socket.write_all(http_resp.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });

    let temp_cache = std::env::temp_dir().join(format!("ironrdp-avd-token-test-{}.json", Uuid::new_v4()));
    let authority = format!("http://{addr}/tenant");

    let provider =
        OAuthAvdTokenProvider::new_loopback_test(AvdOAuthMode::DeviceCode, Some(temp_cache.clone())).unwrap();

    let challenge = AvdGatewayChallenge {
        authority: authority.clone(),
        client_id: DEFAULT_WVD_CLIENT_ID.to_owned(),
        resource: "https://www.wvd.microsoft.com".to_owned(),
        site_id: None,
        username_hint: None,
    };

    let (gw_token, user) = provider.acquire_gateway_token_async(&challenge).await.unwrap();
    assert_eq!(gw_token, "gw-access-token-1");
    assert_eq!(user, "user1@downhillpro.xyz");

    let pop_token = provider
        .acquire_host_pop_token_async(&AvdHostPopTokenRequest {
            authority: authority.clone(),
            client_id: DEFAULT_WVD_CLIENT_ID.to_owned(),
            resource_uri: "ms-device-service://a4a365df-50f1-4397-bc59-1a1564b8bb9c/id/device-1".to_owned(),
            req_cnf: "kid-123".to_owned(),
            username_hint: Some(user.clone()),
        })
        .await
        .unwrap();
    assert_eq!(pop_token, "host-pop-token-1");

    let reloaded =
        OAuthAvdTokenProvider::new_loopback_test(AvdOAuthMode::DeviceCode, Some(temp_cache.clone())).unwrap();
    let (gw_token_2, user_2) = reloaded.acquire_gateway_token_async(&challenge).await.unwrap();
    assert_eq!(gw_token_2, "gw-access-token-2");
    assert_eq!(user_2, "user1@downhillpro.xyz");

    let _ = std::fs::remove_file(&temp_cache);
    server.await.unwrap();
}
