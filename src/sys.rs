use std::fs;
use std::io::{self, Cursor};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::model::parse_vless;

pub const SOCKS_PORT: u16 = 10808;
pub const HTTP_PORT: u16 = 10809;

const CORE_URL: &str = "https://github.com/XTLS/Xray-core/releases/latest/download/Xray-windows-64.zip";
const INET_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn s<E: ToString>(e: E) -> String {
    e.to_string()
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("JustPN");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent("JustPN/0.1")
        .build()
        .into()
}

/// xray.exe рядом с программой имеет приоритет над скачанным в %APPDATA%.
fn core_path() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.with_file_name("xray.exe");
    [beside, data_dir().join("xray.exe")].into_iter().find(|p| p.exists())
}

pub fn ensure_core(status: impl Fn(&str)) -> Result<PathBuf, String> {
    if let Some(p) = core_path() {
        return Ok(p);
    }
    status("Загрузка ядра Xray…");
    let mut resp = http_agent(Duration::from_secs(180)).get(CORE_URL).call().map_err(s)?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(120 * 1024 * 1024)
        .read_to_vec()
        .map_err(s)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(s)?;
    let mut entry = zip.by_name("xray.exe").map_err(s)?;
    let target = data_dir().join("xray.exe");
    let part = target.with_extension("part");
    let mut out = fs::File::create(&part).map_err(s)?;
    io::copy(&mut entry, &mut out).map_err(s)?;
    drop(out);
    fs::rename(&part, &target).map_err(s)?;
    Ok(target)
}

pub fn start_core(exe: &Path, config: &str) -> Result<Child, String> {
    let dir = data_dir();
    let cfg = dir.join("config.json");
    let log_path = dir.join("xray.log");
    fs::write(&cfg, config).map_err(s)?;
    let log = fs::File::create(&log_path).map_err(s)?;
    let mut child = Command::new(exe)
        .args(["run", "-c"])
        .arg(&cfg)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(s)?))
        .stderr(Stdio::from(log))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(s)?;

    let addr = ([127, 0, 0, 1], HTTP_PORT).into();
    for _ in 0..50 {
        if child.try_wait().map_err(s)?.is_some() {
            let tail = fs::read_to_string(&log_path).unwrap_or_default();
            let tail = tail.lines().last().unwrap_or("неизвестная ошибка");
            return Err(format!("Ядро не запустилось: {tail}"));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err("Ядро не ответило за 5 секунд".into())
}

pub fn set_system_proxy(on: bool) -> Result<(), String> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(INET_KEY).map_err(s)?;
    if on {
        key.set_value("ProxyServer", &format!("127.0.0.1:{HTTP_PORT}")).map_err(s)?;
        key.set_value("ProxyOverride", &"<local>;localhost;127.*;10.*;192.168.*").map_err(s)?;
    }
    key.set_value("ProxyEnable", &(on as u32)).map_err(s)?;
    notify_proxy_changed();
    Ok(())
}

/// Если прошлый запуск упал, не оставляем систему с мёртвым прокси.
pub fn clear_stale_proxy() {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(INET_KEY) else { return };
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    let server: String = key.get_value("ProxyServer").unwrap_or_default();
    if enabled == 1 && server == format!("127.0.0.1:{HTTP_PORT}") {
        let _ = set_system_proxy(false);
    }
}

fn notify_proxy_changed() {
    use windows_sys::Win32::Networking::WinInet::{
        INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW,
    };
    unsafe {
        InternetSetOptionW(std::ptr::null_mut(), INTERNET_OPTION_SETTINGS_CHANGED, std::ptr::null(), 0);
        InternetSetOptionW(std::ptr::null_mut(), INTERNET_OPTION_REFRESH, std::ptr::null(), 0);
    }
}

/// Время TCP-рукопожатия до сервера в мс.
pub fn tcp_ping(link: &str) -> String {
    let Some(v) = parse_vless(link) else { return "—".into() };
    let Some(addr) = (v.host.as_str(), v.port).to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return "—".into();
    };
    let t = Instant::now();
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => format!("{} мс", t.elapsed().as_millis()),
        Err(_) => "—".into(),
    }
}

/// Стабильный идентификатор устройства для панелей, ограничивающих число устройств.
/// Считается из MachineGuid, но сам GUID наружу не уходит.
pub fn hwid() -> String {
    let guid: String = RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Cryptography")
        .and_then(|k| k.get_value("MachineGuid"))
        .unwrap_or_else(|_| "justpn-unknown".into());
    let fnv = |seed: u64| {
        guid.bytes()
            .fold(0xcbf29ce484222325 ^ seed, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    };
    format!("{:016x}{:016x}", fnv(0), fnv(0x9e3779b97f4a7c15))
}
