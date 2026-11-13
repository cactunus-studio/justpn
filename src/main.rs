#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod model;
mod sys;

use std::collections::HashMap;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use model::{Server, State};

slint::include_modules!();

struct App {
    state: State,
    child: Option<Child>,
    since: Option<Instant>,
    ping: HashMap<String, String>,
    busy: bool,
}

type Shared = Arc<Mutex<App>>;

fn lock(app: &Shared) -> MutexGuard<'_, App> {
    app.lock().unwrap_or_else(|e| e.into_inner())
}

fn ui_do(ui: &Weak<MainWindow>, f: impl FnOnce(&MainWindow) + Send + 'static) {
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            f(&ui);
        }
    });
}

fn render(app: &Shared, ui: &Weak<MainWindow>) {
    let a = lock(app);
    let st = &a.state;
    let servers: Vec<ServerItem> = st
        .servers
        .iter()
        .map(|s| ServerItem {
            name: s.name.as_str().into(),
            sub: st
                .subs
                .iter()
                .find(|x| x.url == s.sub)
                .map_or("Вручную", |x| x.name.as_str())
                .into(),
            ping: a.ping.get(&s.link).map_or("", String::as_str).into(),
            selected: st.selected.as_ref() == Some(&s.link),
        })
        .collect();
    let subs: Vec<SubItem> = st
        .subs
        .iter()
        .map(|x| SubItem {
            name: x.name.as_str().into(),
            url: x.url.as_str().into(),
            count: st.servers.iter().filter(|s| s.sub == x.url).count() as i32,
        })
        .collect();
    let selected = st.servers.iter().find(|s| st.selected.as_ref() == Some(&s.link));
    let sel_name = selected.map_or(String::new(), |s| s.name.clone());
    let sel_ping = selected.and_then(|s| a.ping.get(&s.link)).cloned().unwrap_or_default();
    drop(a);
    ui_do(ui, move |u| {
        u.set_servers(ModelRc::new(VecModel::from(servers)));
        u.set_subs(ModelRc::new(VecModel::from(subs)));
        u.set_selected_name(sel_name.into());
        u.set_selected_ping(sel_ping.into());
    });
}

static TOAST_GEN: AtomicU64 = AtomicU64::new(0);

fn toast(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let id = TOAST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let text = text.into();
    ui_do(ui, move |u| u.set_message(text.into()));
    let ui = ui.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(3500));
        if TOAST_GEN.load(Ordering::SeqCst) == id {
            ui_do(&ui, |u| u.set_message("".into()));
        }
    });
}

fn set_conn(ui: &Weak<MainWindow>, st: ConnState) {
    ui_do(ui, move |u| u.set_conn_state(st));
}

fn connect(app: &Shared, ui: &Weak<MainWindow>) {
    {
        let mut a = lock(app);
        if a.busy || a.child.is_some() {
            return;
        }
        a.busy = true;
    }
    set_conn(ui, ConnState::Connecting);
    ui_do(ui, |u| u.set_note("Запуск…".into()));

    let result = (|| -> Result<Child, String> {
        let link = lock(app).state.selected.clone().ok_or("Выберите сервер")?;
        let cfg = model::xray_config(&link, sys::SOCKS_PORT, sys::HTTP_PORT)?;
        let exe = sys::ensure_core(|m| {
            let m = m.to_string();
            ui_do(ui, move |u| u.set_note(m.into()));
        })?;
        let mut child = sys::start_core(&exe, &cfg)?;
        if let Err(e) = sys::set_system_proxy(true) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(child)
    })();

    let mut a = lock(app);
    a.busy = false;
    match result {
        Ok(child) => {
            a.child = Some(child);
            a.since = Some(Instant::now());
            drop(a);
            ui_do(ui, |u| u.set_uptime("00:00:00".into()));
            set_conn(ui, ConnState::On);
        }
        Err(e) => {
            drop(a);
            set_conn(ui, ConnState::Off);
            toast(ui, e);
        }
    }
}

fn disconnect(app: &Shared, ui: &Weak<MainWindow>) {
    {
        let mut a = lock(app);
        if let Some(mut c) = a.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        a.since = None;
    }
    let _ = sys::set_system_proxy(false);
    set_conn(ui, ConnState::Off);
}

fn fetch_sub(url: &str) -> Result<(String, Vec<Server>), String> {
    let mut resp = sys::http_agent(Duration::from_secs(20)).get(url).call().map_err(|e| e.to_string())?;
    let title = resp
        .headers()
        .get("profile-title")
        .and_then(|v| v.to_str().ok())
        .map(model::decode_title);
    let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let servers = model::parse_subscription(&body, url);
    let name = title.filter(|t| !t.is_empty()).unwrap_or_else(|| {
        url.split("//").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url).to_string()
    });
    Ok((name, servers))
}

fn add_input(app: &Shared, ui: &Weak<MainWindow>, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let (app, ui) = (app.clone(), ui.clone());
    thread::spawn(move || {
        if text.starts_with("vless://") {
            match model::server_from_link(&text, "") {
                Some(s) => {
                    let mut a = lock(&app);
                    a.state.add_server(s);
                    a.state.save();
                    drop(a);
                    render(&app, &ui);
                    toast(&ui, "Сервер добавлен");
                }
                None => toast(&ui, "Не удалось разобрать ссылку VLESS"),
            }
        } else if text.starts_with("http://") || text.starts_with("https://") {
            toast(&ui, "Загружаю подписку…");
            match fetch_sub(&text) {
                Ok((_, servers)) if servers.is_empty() => toast(&ui, "В подписке нет VLESS-серверов"),
                Ok((name, servers)) => {
                    let n = servers.len();
                    let mut a = lock(&app);
                    a.state.upsert_sub(&text, &name, servers);
                    a.state.save();
                    drop(a);
                    render(&app, &ui);
                    toast(&ui, format!("Загружено серверов: {n}"));
                }
                Err(e) => toast(&ui, format!("Ошибка: {e}")),
            }
        } else {
            toast(&ui, "Нужна ссылка на подписку или vless://");
        }
    });
}

fn refresh_subs(app: &Shared, ui: &Weak<MainWindow>, quiet: bool) {
    let (app, ui) = (app.clone(), ui.clone());
    thread::spawn(move || {
        let urls: Vec<String> = lock(&app).state.subs.iter().map(|s| s.url.clone()).collect();
        if urls.is_empty() {
            return;
        }
        if !quiet {
            toast(&ui, "Обновляю подписки…");
        }
        let (mut ok, mut fail) = (0, 0);
        for url in urls {
            match fetch_sub(&url) {
                Ok((name, servers)) if !servers.is_empty() => {
                    let mut a = lock(&app);
                    a.state.upsert_sub(&url, &name, servers);
                    a.state.save();
                    ok += 1;
                }
                _ => fail += 1,
            }
        }
        render(&app, &ui);
        if !quiet {
            toast(&ui, if fail == 0 { format!("Обновлено: {ok}") } else { format!("Обновлено: {ok}, с ошибкой: {fail}") });
        }
    });
}

fn ping_all(app: &Shared, ui: &Weak<MainWindow>) {
    let (app, ui) = (app.clone(), ui.clone());
    thread::spawn(move || {
        let links: Vec<String> = {
            let mut a = lock(&app);
            let links: Vec<String> = a.state.servers.iter().map(|s| s.link.clone()).collect();
            for l in &links {
                a.ping.insert(l.clone(), "…".into());
            }
            links
        };
        render(&app, &ui);
        for chunk in links.chunks(8) {
            let results: Vec<(String, String)> = thread::scope(|sc| {
                let hs: Vec<_> = chunk.iter().map(|l| sc.spawn(move || (l.clone(), sys::tcp_ping(l)))).collect();
                hs.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            lock(&app).ping.extend(results);
            render(&app, &ui);
        }
    });
}

fn main() {
    sys::clear_stale_proxy();

    let mut state = State::load();
    state.fix_selection();
    let first_run = state.servers.is_empty() && state.subs.is_empty();
    let app: Shared = Arc::new(Mutex::new(App {
        state,
        child: None,
        since: None,
        ping: HashMap::new(),
        busy: false,
    }));

    let win = MainWindow::new().unwrap();
    let ui = win.as_weak();
    if first_run {
        win.set_page(2);
    }
    render(&app, &ui);

    win.on_toggle_connect({
        let (app, ui) = (app.clone(), ui.clone());
        move || {
            let (app, ui) = (app.clone(), ui.clone());
            thread::spawn(move || {
                if lock(&app).child.is_some() {
                    disconnect(&app, &ui);
                } else {
                    connect(&app, &ui);
                }
            });
        }
    });

    win.on_select_server({
        let (app, ui) = (app.clone(), ui.clone());
        move |i| {
            let reconnect = {
                let mut a = lock(&app);
                let Some(link) = a.state.servers.get(i as usize).map(|s| s.link.clone()) else { return };
                if a.state.selected.as_ref() == Some(&link) {
                    return;
                }
                a.state.selected = Some(link);
                a.state.save();
                a.child.is_some()
            };
            render(&app, &ui);
            if reconnect {
                let (app, ui) = (app.clone(), ui.clone());
                thread::spawn(move || {
                    disconnect(&app, &ui);
                    connect(&app, &ui);
                });
            }
        }
    });

    win.on_add({
        let (app, ui) = (app.clone(), ui.clone());
        move |t| add_input(&app, &ui, t.to_string())
    });

    win.on_ping_all({
        let (app, ui) = (app.clone(), ui.clone());
        move || ping_all(&app, &ui)
    });

    win.on_refresh_subs({
        let (app, ui) = (app.clone(), ui.clone());
        move || refresh_subs(&app, &ui, false)
    });

    win.on_remove_sub({
        let (app, ui) = (app.clone(), ui.clone());
        move |i| {
            {
                let mut a = lock(&app);
                let Some(url) = a.state.subs.get(i as usize).map(|s| s.url.clone()) else { return };
                a.state.remove_sub(&url);
                a.state.save();
            }
            render(&app, &ui);
        }
    });

    // Сторож: ловит падение ядра и обновляет таймер.
    {
        let (app, ui) = (app.clone(), ui.clone());
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                let mut a = lock(&app);
                let died = a.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
                if died {
                    a.child = None;
                    a.since = None;
                }
                let up = a.since.map(|t| {
                    let s = t.elapsed().as_secs();
                    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
                });
                drop(a);
                if died {
                    let _ = sys::set_system_proxy(false);
                    set_conn(&ui, ConnState::Off);
                    toast(&ui, "Ядро остановилось");
                } else if let Some(up) = up {
                    ui_do(&ui, move |u| u.set_uptime(up.into()));
                }
            }
        });
    }

    refresh_subs(&app, &ui, true);

    win.run().unwrap();

    // Закрыли окно — возвращаем систему в исходное состояние.
    if let Some(mut c) = lock(&app).child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    let _ = sys::set_system_proxy(false);
}
