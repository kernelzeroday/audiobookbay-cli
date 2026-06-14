use anyhow::{bail, Context, Result};
use clap::Parser;
use reqwest::blocking::Client;
use reqwest::header;
use scraper::{Html, Selector};
use serde::Deserialize;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://audiobookbay.lu";

#[derive(Parser)]
#[command(name = "audiobookbay", about = "Search AudioBookBay for audiobook torrents")]
struct Cli {
    /// Search query
    query: Vec<String>,

    /// Max results to show
    #[arg(short = 'n', long, default_value = "20")]
    limit: usize,

    /// Force fresh login (ignore saved session)
    #[arg(long)]
    relogin: bool,

    /// Number of search result pages to fetch
    #[arg(short = 'p', long, default_value = "1")]
    pages: usize,

    /// Download .torrent files to this directory
    #[arg(short = 't', long)]
    torrent_dir: Option<PathBuf>,

    /// Override base URL (e.g. https://audiobookbay.nl)
    #[arg(long, env = "AUDIOBOOKBAY_URL")]
    base_url: Option<String>,

    /// Suppress progress output
    #[arg(short, long)]
    quiet: bool,

    /// Number of retries for failed HTTP requests (with exponential backoff)
    #[arg(long, default_value = "3")]
    retries: u32,

    /// HTTP/SOCKS5 proxy URL (HTTPS_PROXY / ALL_PROXY env vars also respected)
    #[arg(long)]
    proxy: Option<String>,
}

#[derive(Deserialize)]
struct Config {
    username: String,
    password: String,
    #[serde(default)]
    base_url: Option<String>,
}

struct Audiobook {
    title: String,
    detail_url: String,
    format: String,
    size: String,
    lang: String,
    magnet: String,
    torrent_url: String,
}

struct DetailInfo {
    info_hash: String,
    trackers: Vec<String>,
    torrent_url: String,
}

const FALLBACK_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://p4p.arenabg.com:1337/announce",
    "udp://bt1.archive.org:6969/announce",
    "http://tracker.bt4g.com:2095/announce",
    "http://bt.okmp3.ru:2710/announce",
    "http://tracker2.dler.org:80/announce",
];

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("audiobookbay")
}

fn session_path() -> PathBuf {
    config_dir().join("session")
}

fn load_config() -> Result<Config> {
    let path = config_dir().join("config.toml");

    let mut config = if let Ok(content) = fs::read_to_string(&path) {
        toml::from_str::<Config>(&content).context("Failed to parse config.toml")?
    } else {
        let user = std::env::var("AUDIOBOOKBAY_USER");
        let pass = std::env::var("AUDIOBOOKBAY_PASS");
        match (user, pass) {
            (Ok(username), Ok(password)) => Config {
                username,
                password,
                base_url: None,
            },
            _ => bail!(
                "Config not found at {path}\n\n\
                 Create it:\n  mkdir -p {dir}\n  \
                 cat > {path} << 'EOF'\n  \
                 username = \"your_username\"\n  \
                 password = \"your_password\"\n  \
                 # base_url = \"https://audiobookbay.nl\"\n  \
                 EOF\n\n\
                 Or set AUDIOBOOKBAY_USER / AUDIOBOOKBAY_PASS / AUDIOBOOKBAY_URL env vars.",
                path = path.display(),
                dir = config_dir().display(),
            ),
        }
    };

    if let Ok(u) = std::env::var("AUDIOBOOKBAY_USER") {
        config.username = u;
    }
    if let Ok(p) = std::env::var("AUDIOBOOKBAY_PASS") {
        config.password = p;
    }

    Ok(config)
}

fn save_session(cookie: &str) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    fs::write(session_path(), cookie)?;
    Ok(())
}

fn load_session() -> Option<String> {
    fs::read_to_string(session_path())
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn build_client(
    base_url: &str,
    session_cookie: Option<&str>,
    proxy: Option<&str>,
) -> Result<Client> {
    let jar = Arc::new(reqwest::cookie::Jar::default());

    if let Some(cookie) = session_cookie {
        let url: reqwest::Url = base_url.parse().context("Invalid base URL")?;
        jar.add_cookie_str(&format!("PHPSESSID={}", cookie), &url);
    }

    let mut builder = Client::builder()
        .cookie_provider(jar)
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10));

    if let Some(proxy_url) = proxy {
        builder = builder
            .no_proxy()
            .proxy(reqwest::Proxy::all(proxy_url).context("Invalid proxy URL")?);
    }

    builder.build().context("Failed to build HTTP client")
}

fn is_logged_in(html: &str) -> bool {
    html.contains("member/logout") || html.contains("You are logged in")
}

fn login(base_url: &str, config: &Config, proxy: Option<&str>) -> Result<String> {
    let mut builder = Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10));

    if let Some(proxy_url) = proxy {
        builder = builder
            .no_proxy()
            .proxy(reqwest::Proxy::all(proxy_url).context("Invalid proxy URL")?);
    }

    let login_client = builder.build()?;

    let body = format!(
        "username={}&password={}",
        urlencoding(&config.username),
        urlencoding(&config.password),
    );

    let resp = login_client
        .post(format!("{}/member/login.php", base_url))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .context("Login request failed — is audiobookbay reachable?")?;

    let session: Option<String> = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("PHPSESSID="))
        .and_then(|s| {
            s.strip_prefix("PHPSESSID=")
                .and_then(|rest| rest.split(';').next())
                .map(String::from)
        });

    let status = resp.status();

    if let Some(ref session) = session {
        if !session.is_empty() && status.is_redirection() {
            return Ok(session.clone());
        }
    }

    let html = resp.text().unwrap_or_default();
    if html.contains("Wrong username or password") || html.contains("login-input") {
        bail!(
            "Login failed — check credentials in {}",
            config_dir().join("config.toml").display()
        );
    }

    if let Some(session) = session {
        if !session.is_empty() {
            return Ok(session);
        }
    }

    bail!(
        "Login failed — no session cookie received (status {})",
        status
    );
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn with_retry<F, T>(retries: u32, quiet: bool, f: F) -> Result<T>
where
    F: Fn() -> Result<T>,
{
    let mut last_err = None;
    for attempt in 0..=retries {
        match f() {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt < retries {
                    let delay = Duration::from_secs(2u64.pow(attempt.min(5)));
                    if !quiet {
                        eprintln!(
                            "  retry {}/{} in {}s: {}",
                            attempt + 1,
                            retries,
                            delay.as_secs(),
                            e
                        );
                    }
                    thread::sleep(delay);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}

fn ensure_logged_in(
    base_url: &str,
    config: &Config,
    force_login: bool,
    quiet: bool,
    proxy: Option<&str>,
) -> Result<Client> {
    if !force_login {
        if let Some(session) = load_session() {
            let client = build_client(base_url, Some(&session), proxy)?;
            if !quiet {
                eprint!("Checking session... ");
            }
            match client.get(format!("{}/", base_url)).send() {
                Ok(resp) => {
                    let html = resp.text().unwrap_or_default();
                    if is_logged_in(&html) {
                        if !quiet {
                            eprintln!("ok");
                        }
                        return Ok(client);
                    }
                    if !quiet {
                        eprintln!("expired");
                    }
                }
                Err(_) => {
                    if !quiet {
                        eprintln!("failed");
                    }
                }
            }
        }
    }

    if !quiet {
        eprint!("Logging in... ");
    }
    let session = login(base_url, config, proxy)?;
    let _ = save_session(&session);
    let client = build_client(base_url, Some(&session), proxy)?;
    if !quiet {
        eprintln!("ok");
    }

    Ok(client)
}

fn decode_base64_posts(html: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let mut result = html.to_string();
    let marker = "class=\"post re-ab\" style=\"display:none;\">";

    while let Some(start) = result.find(marker) {
        let content_start = start + marker.len();
        if let Some(end_offset) = result[content_start..].find("</div>") {
            let b64 = &result[content_start..content_start + end_offset];
            if let Ok(decoded) = STANDARD.decode(b64.trim()) {
                if let Ok(html_str) = String::from_utf8(decoded) {
                    let replacement = format!("class=\"post\">{}", html_str);
                    result.replace_range(start..content_start + end_offset, &replacement);
                    continue;
                }
            }
        }
        break;
    }

    result
}

fn search(client: &Client, base_url: &str, query: &str, page: usize) -> Result<String> {
    let url = if page > 1 {
        format!("{}/page/{}/?s={}", base_url, page, urlencoding(query))
    } else {
        format!("{}/?s={}", base_url, urlencoding(query))
    };

    let resp = client.get(&url).send().context("Search request failed")?;
    let final_url = resp.url().to_string();
    let html = resp.text()?;

    // Redirected away from search — site returned homepage instead of results
    if !final_url.contains("?s=") && !final_url.contains("&s=") {
        return Ok(String::new());
    }

    let lower = html.to_lowercase();
    if lower.contains("nothing found") || lower.contains("no post found") {
        return Ok(String::new());
    }

    Ok(decode_base64_posts(&html))
}

fn parse_results(html: &str, limit: usize) -> Vec<Audiobook> {
    let doc = Html::parse_document(html);
    let post_sel = Selector::parse("div.post").unwrap();
    let title_sel = Selector::parse("div.postTitle h2 a").unwrap();
    let info_sel = Selector::parse("div.postInfo").unwrap();
    let content_sel = Selector::parse("div.postContent").unwrap();

    let mut results = Vec::new();

    for post in doc.select(&post_sel) {
        if results.len() >= limit {
            break;
        }

        let (title, detail_url) = match post.select(&title_sel).next() {
            Some(el) => {
                let t: String = el.text().collect();
                let t = t.trim().to_string();
                if t.is_empty() {
                    continue;
                }
                let href = el.value().attr("href").unwrap_or("").to_string();
                (t, href)
            }
            None => continue,
        };

        let lang = post
            .select(&info_sel)
            .next()
            .map(|el| {
                let text: String = el.text().collect();
                text.split("Language:")
                    .nth(1)
                    .and_then(|s| s.split("Keywords:").next())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let (format, size) = post
            .select(&content_sel)
            .next()
            .map(|el| {
                let text: String = el.text().collect();
                let fmt = text
                    .split("Format:")
                    .nth(1)
                    .and_then(|s| s.split('/').next())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let sz = text
                    .split("File Size:")
                    .nth(1)
                    .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                    .unwrap_or_default();
                (fmt, sz)
            })
            .unwrap_or_default();

        results.push(Audiobook {
            title,
            detail_url,
            format,
            size,
            lang,
            magnet: String::new(),
            torrent_url: String::new(),
        });
    }

    results
}

fn fetch_detail_info(client: &Client, base_url: &str, detail_path: &str) -> Result<DetailInfo> {
    let url = if detail_path.starts_with("http") {
        detail_path.to_string()
    } else {
        format!("{}{}", base_url, detail_path)
    };

    let resp = client.get(&url).send()?;
    let html = resp.text()?;

    let doc = Html::parse_document(&html);
    let td_sel = Selector::parse("td").unwrap();

    let mut info_hash = String::new();
    let mut trackers = Vec::new();

    let tds: Vec<_> = doc.select(&td_sel).collect();
    for (i, td) in tds.iter().enumerate() {
        let text: String = td.text().collect();
        if text.contains("Info Hash") {
            if let Some(next_td) = tds.get(i + 1) {
                let hash: String = next_td.text().collect::<String>().trim().to_string();
                if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    info_hash = hash;
                }
            }
        }
        if text.contains("Tracker") && !text.contains("Info Hash") {
            if let Some(next_td) = tds.get(i + 1) {
                let tracker_text: String = next_td.text().collect();
                for line in tracker_text.lines() {
                    let t = line.trim();
                    if t.starts_with("udp://")
                        || t.starts_with("http://")
                        || t.starts_with("https://")
                    {
                        trackers.push(t.to_string());
                    }
                }
            }
        }
    }

    if info_hash.is_empty() {
        bail!("Info hash not found on detail page");
    }

    let a_sel = Selector::parse("a").unwrap();
    let mut torrent_url = String::new();
    for a in doc.select(&a_sel) {
        if let Some(href) = a.value().attr("href") {
            if href.contains(".torrent") || href.contains("/torrent/") {
                torrent_url = if href.starts_with("http") {
                    href.to_string()
                } else {
                    format!("{}{}", base_url, href)
                };
                break;
            }
        }
    }

    Ok(DetailInfo {
        info_hash,
        trackers,
        torrent_url,
    })
}

fn magnet_from_hash(hash: &str, title: &str, trackers: &[String]) -> String {
    let mut magnet = format!("magnet:?xt=urn:btih:{}&dn={}", hash, urlencoding(title));

    let tr_list: &[String];
    let fallback: Vec<String>;
    if trackers.is_empty() {
        fallback = FALLBACK_TRACKERS.iter().map(|s| s.to_string()).collect();
        tr_list = &fallback;
    } else {
        tr_list = trackers;
    }

    for tr in tr_list {
        magnet.push_str("&tr=");
        magnet.push_str(&urlencoding(tr));
    }

    magnet
}

fn download_torrent(
    client: &Client,
    url: &str,
    dir: &std::path::Path,
    title: &str,
) -> Result<PathBuf> {
    let resp = client
        .get(url)
        .send()
        .context("Failed to download torrent")?;

    let filename = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split("filename=")
                .nth(1)
                .map(|f| f.trim_matches('"').to_string())
        })
        .unwrap_or_else(|| {
            let safe: String = title
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == ' ' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!("{}.torrent", safe.trim())
        });

    let path = dir.join(&filename);
    let bytes = resp.bytes()?;
    let mut file = fs::File::create(&path)?;
    file.write_all(&bytes)?;
    Ok(path)
}

fn display(results: &[Audiobook]) {
    if results.is_empty() {
        eprintln!("No results found.");
        return;
    }

    for t in results {
        println!("{}", t.title);
        let mut meta = Vec::new();
        if !t.size.is_empty() {
            meta.push(format!("size={}", t.size));
        }
        if !t.format.is_empty() {
            meta.push(format!("format={}", t.format));
        }
        if !t.lang.is_empty() {
            meta.push(format!("lang={}", t.lang));
        }
        if !meta.is_empty() {
            println!("  {}", meta.join(" "));
        }
        if !t.magnet.is_empty() {
            println!("  {}", t.magnet);
        }
        println!();
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let quiet = cli.quiet;
    let is_tty = std::io::stderr().is_terminal();

    if cli.query.is_empty() {
        bail!("Usage: audiobookbay <query>\n\nProvide a search query.");
    }

    let mut config = load_config()?;
    let base_url = cli
        .base_url
        .or(config.base_url.take())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let base_url = base_url.trim_end_matches('/').to_string();

    let proxy = cli.proxy.as_deref();
    let client = ensure_logged_in(&base_url, &config, cli.relogin, quiet, proxy)?;

    let query = cli.query.join(" ");

    let mut all_results: Vec<Audiobook> = Vec::new();

    for page in 1..=cli.pages {
        if !quiet {
            if cli.pages > 1 {
                eprint!("Searching page {}/{}... ", page, cli.pages);
            } else {
                eprint!("Searching... ");
            }
        }
        let html = with_retry(cli.retries, quiet, || {
            search(&client, &base_url, &query, page)
        })?;
        let results = parse_results(&html, cli.limit.saturating_sub(all_results.len()));
        let count = results.len();
        all_results.extend(results);
        if !quiet {
            eprintln!("{} results", count);
        }

        if count == 0 || all_results.len() >= cli.limit {
            break;
        }
    }

    if all_results.is_empty() {
        eprintln!("No results found for: {query}");
        return Ok(());
    }

    let torrent_dir = cli.torrent_dir.as_deref();
    if let Some(dir) = torrent_dir {
        fs::create_dir_all(dir).context("Failed to create torrent directory")?;
    }

    let total = all_results.len();
    for (i, book) in all_results.iter_mut().enumerate() {
        if book.detail_url.is_empty() {
            continue;
        }
        if !quiet && is_tty {
            eprint!("\rFetching details ({}/{})... ", i + 1, total);
        }
        let detail_url = book.detail_url.clone();
        let detail_result = with_retry(cli.retries, quiet, || {
            fetch_detail_info(&client, &base_url, &detail_url)
        });
        match detail_result {
            Ok(detail) => {
                book.magnet =
                    magnet_from_hash(&detail.info_hash, &book.title, &detail.trackers);
                book.torrent_url = detail.torrent_url;

                if let Some(dir) = torrent_dir {
                    if !book.torrent_url.is_empty() {
                        let torrent_url = book.torrent_url.clone();
                        let title = book.title.clone();
                        match with_retry(cli.retries, quiet, || {
                            download_torrent(&client, &torrent_url, dir, &title)
                        }) {
                            Ok(path) => {
                                if !quiet {
                                    if is_tty {
                                        eprintln!("\r  Saved: {}", path.display());
                                    } else {
                                        eprintln!("  Saved: {}", path.display());
                                    }
                                }
                            }
                            Err(e) => {
                                if !quiet {
                                    if is_tty {
                                        eprintln!("\r  Torrent download failed: {}", e);
                                    } else {
                                        eprintln!("  Torrent download failed: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if !quiet {
                    if is_tty {
                        eprintln!("\r  Detail fetch failed: {}", e);
                    } else {
                        eprintln!("  Detail fetch failed: {}", e);
                    }
                }
            }
        }
    }
    if !quiet && total > 0 {
        if is_tty {
            eprintln!("\rFetched {} details.{}", total, " ".repeat(20));
        } else {
            eprintln!("Fetched {} details.", total);
        }
    }

    println!();
    display(&all_results);

    Ok(())
}
