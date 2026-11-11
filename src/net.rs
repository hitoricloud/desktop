//! Blocking HTTP, run on the background executor.
use std::io::Read;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Geo {
    pub lat: f64,
    pub lon: f64,
    pub city: Option<String>,
    pub country: Option<String>,
    /// ISO 3166-1 alpha-2, upper case.
    pub code: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent("hitori/0.1").build()
}

fn json(url: &str) -> Option<serde_json::Value> {
    agent().get(url).call().ok()?.into_json().ok()
}

pub fn my_ip() -> Option<String> {
    json("https://stat.ripe.net/data/whats-my-ip/data.json")?["data"]["ip"].as_str().map(str::to_owned)
}

fn s(v: &serde_json::Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn ipwho(ip: &str) -> Option<Geo> {
    let d = json(&format!("https://ipwho.is/{ip}"))?;
    if d["success"].as_bool() != Some(true) {
        return None;
    }
    Some(Geo { lat: d["latitude"].as_f64()?, lon: d["longitude"].as_f64()?, city: s(&d["city"]), country: s(&d["country"]), code: code(&d["country_code"]) })
}

fn code(v: &serde_json::Value) -> Option<String> {
    s(v).filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase())
}

fn geojs(ip: &str) -> Option<Geo> {
    let d = json(&format!("https://get.geojs.io/v1/ip/geo/{ip}.json"))?;
    let num = |v: &serde_json::Value| v.as_str().and_then(|x| x.parse::<f64>().ok()).or_else(|| v.as_f64());
    Some(Geo { lat: num(&d["latitude"])?, lon: num(&d["longitude"])?, city: s(&d["city"]), country: s(&d["country"]), code: code(&d["country_code"]) })
}

/// ipwho.is and geojs.io in parallel; ipwho.is wins when both answer.
pub fn locate(ip: &str) -> Option<Geo> {
    let ip2 = ip.to_owned();
    let other = std::thread::spawn(move || geojs(&ip2));
    let first = ipwho(ip);
    let second = other.join().ok().flatten();
    first.or(second)
}

pub fn bytes(url: &str) -> Option<Vec<u8>> {
    download(url, 4 << 20)
}

/// At most `limit` bytes of a successful response.
pub fn download(url: &str, limit: u64) -> Option<Vec<u8>> {
    let resp = agent().get(url).call().ok()?;
    let mut buf = Vec::new();
    resp.into_reader().take(limit).read_to_end(&mut buf).ok()?;
    Some(buf)
}
