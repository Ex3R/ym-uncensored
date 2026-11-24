//! A separate, revocable session for the online sources. Music credentials never leave Yandex.
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use ring::{rand::{SecureRandom, SystemRandom}, signature::{Ed25519KeyPair, KeyPair}};
use sha2::{Digest, Sha256};
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

static SESSION_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn session_revision() -> u64 {
    SESSION_REVISION.load(std::sync::atomic::Ordering::Acquire)
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
    options.open(dir.join(NAME)).and_then(|mut f| f.write_all(value.as_bytes())).map_err(|e| e.to_string())?;
    SESSION_REVISION.fetch_add(1, std::sync::atomic::Ordering::Release);
    Ok(())
}

fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i+2)?,16).ok()).collect()
}
fn install_key() -> Result<&'static Ed25519KeyPair, String> {
    static KEY: OnceLock<Result<Ed25519KeyPair, String>> = OnceLock::new();
    KEY.get_or_init(|| {
        let name = "external-install-key";
        let path = crate::config::dir().join(name);
        if let Ok(value) = std::fs::read_to_string(&path) {
            let value = crate::secret::open(&value).or_else(|| cfg!(any(target_os="linux",target_os="android")).then_some(value)).ok_or("Ключ установки недоступен")?;
            return Ed25519KeyPair::from_pkcs8(&unhex(&value).ok_or("Неверный ключ установки")?).map_err(|_| "Неверный ключ установки".into());
        }
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| "Не удалось создать ключ установки")?;
        let plain = hex(doc.as_ref());
        let value = crate::secret::seal(name,&plain).or_else(|| cfg!(any(target_os="linux",target_os="android")).then_some(plain)).ok_or("Не удалось сохранить ключ установки")?;
        std::fs::create_dir_all(crate::config::dir()).map_err(|e| e.to_string())?;
        let mut options=std::fs::OpenOptions::new(); options.write(true).create_new(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        options.open(path).and_then(|mut f| f.write_all(value.as_bytes())).map_err(|e| e.to_string())?;
        Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| "Неверный ключ установки".into())
    }).as_ref().map_err(Clone::clone)
}
pub fn signed(method: &str, path: &str, token: &str, body: Vec<u8>) -> Result<reqwest::blocking::RequestBuilder, String> {
    let key = install_key()?;
    let time=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs().to_string();
    let mut nonce=[0u8;16]; SystemRandom::new().fill(&mut nonce).map_err(|_| "Нет источника случайных чисел")?;
    let nonce=hex(&nonce);
    let full_path=format!("/api/uncensored/v1{path}");
    let canonical=[method,&full_path,&hex(&Sha256::digest(&body)),&time,&nonce,&hex(&Sha256::digest(token.as_bytes()))].join("\n");
    let method=reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    Ok(http()?.request(method,format!("{BASE}{path}")).bearer_auth(token).header("X-YM-Time",time)
        .header("X-YM-Nonce",nonce).header("X-YM-Proof",hex(key.sign(canonical.as_bytes()).as_ref()))
        .header("Content-Type","application/octet-stream").body(body))
}

pub fn disconnect() {
    let mut state = STATE.lock().unwrap();
    state.generation = state.generation.wrapping_add(1);
    state.pending = false;
    let token = credential();
    let _ = std::fs::remove_file(crate::config::dir().join(NAME));
    crate::secret::forget(NAME);
    SESSION_REVISION.fetch_add(1, std::sync::atomic::Ordering::Release);
    if let Some(token) = token {
        std::thread::spawn(move || {
            if let Ok(request) = signed("POST", "/auth/logout", &token, vec![]) { let _ = request.send(); }
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
    let body = serde_json::to_vec(&serde_json::json!({"public_key":hex(install_key()?.public_key().as_ref())})).map_err(|e| e.to_string())?;
    let pair = signed("POST", "/auth/start", "", body)?.send().and_then(|r| r.error_for_status())
        .and_then(|r| r.json::<Pairing>()).map_err(|_| "Сервис входа недоступен")?;
    if !valid(&pair.token) || !pair.link.starts_with(&format!("{BASE}/auth/browser?")) { return Err("Неверный ответ сервиса".into()); }
    crate::sys::open_url(&pair.link);
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(3));
        if STATE.lock().unwrap().generation != generation { return Err("Вход отменён".into()); }
        if let Ok(r) = signed("GET", "/auth/status", &pair.token, vec![])?.send() {
            if matches!(r.status(), reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::GONE) {
                return Err("Подключение отменено. Можно пользоваться обычным плеером".into());
            }
            if r.status() == reqwest::StatusCode::NO_CONTENT {
                let state = STATE.lock().unwrap();
                if state.generation == generation { return save(&pair.token); }
                return Err("Вход отменён".into());
            }
        }
    }
    Err("Время входа истекло".into())
}
