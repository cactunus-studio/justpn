use base64::{Engine, engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD}};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;

use crate::sys;

#[derive(Clone, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub link: String,
    /// URL подписки, из которой пришёл сервер; пусто — добавлен вручную.
    pub sub: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Sub {
    pub name: String,
    pub url: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub subs: Vec<Sub>,
    pub servers: Vec<Server>,
    pub selected: Option<String>,
}

impl State {
    pub fn load() -> State {
        fs::read_to_string(sys::data_dir().join("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = fs::write(sys::data_dir().join("state.json"), s);
        }
    }

    /// Выбранный сервер должен существовать; иначе берём первый.
    pub fn fix_selection(&mut self) {
        let ok = self
            .selected
            .as_ref()
            .is_some_and(|l| self.servers.iter().any(|s| &s.link == l));
        if !ok {
            self.selected = self.servers.first().map(|s| s.link.clone());
        }
    }

    pub fn upsert_sub(&mut self, url: &str, name: &str, servers: Vec<Server>) {
        self.servers.retain(|s| s.sub != url);
        self.servers.extend(servers);
        match self.subs.iter_mut().find(|s| s.url == url) {
            Some(s) => s.name = name.to_string(),
            None => self.subs.push(Sub { name: name.to_string(), url: url.to_string() }),
        }
        self.fix_selection();
    }

    pub fn add_server(&mut self, server: Server) {
        if !self.servers.iter().any(|s| s.link == server.link) {
            self.servers.push(server);
        }
        self.fix_selection();
    }

    pub fn remove_sub(&mut self, url: &str) {
        self.subs.retain(|s| s.url != url);
        self.servers.retain(|s| s.sub != url);
        self.fix_selection();
    }
}

pub struct Vless {
    pub uuid: String,
    pub host: String,
    pub port: u16,
    pub q: HashMap<String, String>,
    pub name: String,
}

fn dec(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

pub fn parse_vless(link: &str) -> Option<Vless> {
    let rest = link.trim().strip_prefix("vless://")?;
    let (rest, frag) = rest.split_once('#').unwrap_or((rest, ""));
    let (auth, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (uuid, hostport) = auth.rsplit_once('@')?;
    let (host, port) = match hostport.strip_prefix('[') {
        Some(r) => r.split_once("]:")?,
        None => hostport.rsplit_once(':')?,
    };
    let port: u16 = port.parse().ok()?;
    let q = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), dec(v)))
        .collect();
    let name = match dec(frag).trim() {
        "" => format!("{host}:{port}"),
        n => n.to_string(),
    };
    Some(Vless { uuid: dec(uuid), host: host.to_string(), port, q, name })
}

pub fn server_from_link(link: &str, sub: &str) -> Option<Server> {
    let v = parse_vless(link)?;
    Some(Server { name: v.name, link: link.trim().to_string(), sub: sub.to_string() })
}

/// Тело подписки — обычно base64 (с паддингом или без, обычный или url-safe), иногда просто список ссылок.
pub fn parse_subscription(body: &str, sub: &str) -> Vec<Server> {
    let text = if body.contains("://") {
        body.to_string()
    } else {
        let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let compact = compact.trim_end_matches('=');
        STANDARD_NO_PAD
            .decode(compact)
            .or_else(|_| URL_SAFE_NO_PAD.decode(compact))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    text.lines().filter_map(|l| server_from_link(l, sub)).collect()
}

pub fn decode_title(raw: &str) -> String {
    match raw.strip_prefix("base64:") {
        Some(b) => STANDARD_NO_PAD
            .decode(b.trim_end_matches('='))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| raw.to_string()),
        None => raw.to_string(),
    }
}

pub fn xray_config(link: &str, socks: u16, http: u16) -> Result<String, String> {
    let v = parse_vless(link).ok_or("Некорректная ссылка VLESS")?;
    let g = |k: &str| v.q.get(k).map(String::as_str).unwrap_or("");

    let net = match g("type") {
        "" | "tcp" => "tcp",
        "ws" => "ws",
        "grpc" => "grpc",
        "httpupgrade" => "httpupgrade",
        "xhttp" | "splithttp" => "xhttp",
        other => return Err(format!("Транспорт «{other}» не поддерживается")),
    };
    let sec = match g("security") {
        "" | "none" => "none",
        "tls" => "tls",
        "reality" => "reality",
        other => return Err(format!("Защита «{other}» не поддерживается")),
    };
    let path = if g("path").is_empty() { "/" } else { g("path") };
    let sni = [g("sni"), g("host"), v.host.as_str()].into_iter().find(|s| !s.is_empty()).unwrap();
    let fp = if g("fp").is_empty() { "chrome" } else { g("fp") };

    let mut stream = json!({ "network": net, "security": sec });
    match net {
        "ws" => {
            stream["wsSettings"] = json!({ "path": path, "headers": { "Host": g("host") } });
        }
        "grpc" => {
            stream["grpcSettings"] =
                json!({ "serviceName": g("serviceName"), "multiMode": g("mode") == "multi" });
        }
        "httpupgrade" => {
            stream["httpupgradeSettings"] = json!({ "path": path, "host": g("host") });
        }
        "xhttp" => {
            let mut x = json!({ "path": path, "host": g("host") });
            if !g("mode").is_empty() {
                x["mode"] = json!(g("mode"));
            }
            stream["xhttpSettings"] = x;
        }
        _ => {}
    }
    match sec {
        "tls" => {
            let mut t = json!({
                "serverName": sni,
                "fingerprint": fp,
                "allowInsecure": matches!(g("allowInsecure"), "1" | "true"),
            });
            if !g("alpn").is_empty() {
                t["alpn"] = json!(g("alpn").split(',').collect::<Vec<_>>());
            }
            stream["tlsSettings"] = t;
        }
        "reality" => {
            stream["realitySettings"] = json!({
                "serverName": sni,
                "fingerprint": fp,
                "publicKey": g("pbk"),
                "shortId": g("sid"),
                "spiderX": g("spx"),
            });
        }
        _ => {}
    }

    let mut user = json!({
        "id": v.uuid,
        "encryption": if g("encryption").is_empty() { "none" } else { g("encryption") },
    });
    if !g("flow").is_empty() {
        user["flow"] = json!(g("flow"));
    }

    let cfg: Value = json!({
        "log": { "loglevel": "warning" },
        "inbounds": [
            { "tag": "socks", "listen": "127.0.0.1", "port": socks, "protocol": "socks", "settings": { "udp": true } },
            { "tag": "http", "listen": "127.0.0.1", "port": http, "protocol": "http" }
        ],
        "outbounds": [
            {
                "tag": "proxy",
                "protocol": "vless",
                "settings": { "vnext": [{ "address": v.host, "port": v.port, "users": [user] }] },
                "streamSettings": stream
            },
            { "tag": "direct", "protocol": "freedom" }
        ]
    });
    serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())
}
