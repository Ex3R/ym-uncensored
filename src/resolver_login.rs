//! A separate, revocable session for the online sources. Music credentials never leave Yandex.
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const BASE: &str = "https://ym.idkroma.ru/api/uncensored/v1";
const NAME: &str = "external-sources";
static STATE: Mutex<State> = Mutex::new(State { pending: false, generation: 0 });

struct State {
    pending: bool,
    generation: u64,
}

pub fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build().map_err(|e| e.to_string())
}

fn valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn credential() -> Option<String> {
    let value = std::fs::read_to_string(crate::config::dir().join(NAME)).ok()?;
    let token = if cfg!(any(target_os = "linux", target_os = "android")) && valid(&value) {
        Some(value)
    } else {
        crate::secret::open(&value)
    };
    token.filter(|s| valid(s))
}

fn save(token: &str) -> Result<(), String> {
    let sealed = crate::secret::seal(NAME, token);
    let value = sealed.as_deref().or_else(|| cfg!(any(target_os = "linux", target_os = "android")).then_some(token))
        .ok_or("Не удалось сохранить ключ доступа")?;
    let dir = crate::config::dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    options.open(dir.join(NAME)).and_then(|mut f| f.write_all(value.as_bytes())).map_err(|e| e.to_string())
}

pub fn disconnect() {
    let mut state = STATE.lock().unwrap();
    state.generation = state.generation.wrapping_add(1);
    state.pending = false;
    let token = credential();
    let _ = std::fs::remove_file(crate::config::dir().join(NAME));
    crate::secret::forget(NAME);
    if let Some(token) = token {
        std::thread::spawn(move || {
            if let Ok(client) = http() { let _ = client.post(format!("{BASE}/auth/logout")).bearer_auth(token).send(); }
        });
    }
}

pub fn connect(done: impl FnOnce(Result<(), String>) + Send + 'static) {
    let mut state = STATE.lock().unwrap();
    if state.pending { return; }
    state.pending = true;
    let generation = state.generation;
    std::thread::spawn(move || {
        let result = authorize(generation);
        let mut state = STATE.lock().unwrap();
        if state.generation == generation {
            state.pending = false;
            drop(state);
            done(result);
        }
    });
}

#[derive(serde::Deserialize)]
struct Pairing { token: String, link: String }

fn authorize(generation: u64) -> Result<(), String> {
    let client = http()?;
    let pair = client.post(format!("{BASE}/auth/start")).send().and_then(|r| r.error_for_status())
        .and_then(|r| r.json::<Pairing>()).map_err(|_| "Сервис входа недоступен")?;
    if !valid(&pair.token) || !pair.link.starts_with(&format!("{BASE}/auth/browser?")) { return Err("Неверный ответ сервиса".into()); }
    crate::sys::open_url(&pair.link);
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(3));
        if STATE.lock().unwrap().generation != generation { return Err("Вход отменён".into()); }
        if let Ok(r) = client.get(format!("{BASE}/auth/status")).bearer_auth(&pair.token).send() {
            if r.status().is_success() {
                let state = STATE.lock().unwrap();
                if state.generation == generation { return save(&pair.token); }
                return Err("Вход отменён".into());
            }
        }
    }
    Err("Время входа истекло".into())
}
