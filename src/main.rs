use anyhow::{bail, Context, Result};
use clap::Parser;
use reqwest::blocking::Client;
use reqwest::header;
use scraper::{Html, Selector};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
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
}

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
                 EOF\n\n\
                 Or set AUDIOBOOKBAY_USER and AUDIOBOOKBAY_PASS env vars.",
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

fn build_client(base_url: &str, session_cookie: Option<&str>) -> Result<Client> {
    let jar = Arc::new(reqwest::cookie::Jar::default());

    if let Some(cookie) = session_cookie {
        let url: reqwest::Url = base_url.parse().context("Invalid base URL")?;
        jar.add_cookie_str(&format!("PHPSESSID={}", cookie), &url);
    }

    Client::builder()
        .cookie_provider(jar)
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")
}

fn is_logged_in(html: &str) -> bool {
    html.contains("member/logout") || html.contains("You are logged in")
}

fn login(base_url: &str, config: &Config) -> Result<String> {
    let login_client = Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10))
        .build()?;

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

    bail!("Login failed — no session cookie received (status {})", status);
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

fn ensure_logged_in(base_url: &str, config: &Config, force_login: bool) -> Result<Client> {
    if !force_login {
        if let Some(session) = load_session() {
            let client = build_client(base_url, Some(&session))?;
            eprint!("Checking session... ");
            match client.get(format!("{}/", base_url)).send() {
                Ok(resp) => {
                    let html = resp.text().unwrap_or_default();
                    if is_logged_in(&html) {
                        eprintln!("ok");
                        return Ok(client);
                    }
                    eprintln!("expired");
                }
                Err(_) => {
                    eprintln!("failed");
                }
            }
        }
    }

    eprint!("Logging in... ");
    let session = login(base_url, config)?;
    let _ = save_session(&session);
    let client = build_client(base_url, Some(&session))?;
    eprintln!("ok");

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
    let html = resp.text()?;
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
        });
    }

    results
}

fn fetch_info_hash(client: &Client, base_url: &str, detail_path: &str) -> Result<String> {
    let url = if detail_path.starts_with("http") {
        detail_path.to_string()
    } else {
        format!("{}{}", base_url, detail_path)
    };

    let resp = client.get(&url).send()?;
    let html = resp.text()?;

    let doc = Html::parse_document(&html);
    let td_sel = Selector::parse("td").unwrap();

    let tds: Vec<_> = doc.select(&td_sel).collect();
    for (i, td) in tds.iter().enumerate() {
        let text: String = td.text().collect();
        if text.contains("Info Hash") {
            if let Some(next_td) = tds.get(i + 1) {
                let hash: String = next_td.text().collect::<String>().trim().to_string();
                if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(hash);
                }
            }
        }
    }

    bail!("Info hash not found on detail page")
}

fn magnet_from_hash(hash: &str, title: &str) -> String {
    format!(
        "magnet:?xt=urn:btih:{}&dn={}",
        hash,
        urlencoding(title)
    )
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

    if cli.query.is_empty() {
        bail!("Usage: audiobookbay <query>\n\nProvide a search query.");
    }

    let config = load_config()?;
    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string();

    let client = ensure_logged_in(&base_url, &config, cli.relogin)?;

    let query = cli.query.join(" ");

    let mut all_results: Vec<Audiobook> = Vec::new();

    for page in 1..=cli.pages {
        if cli.pages > 1 {
            eprint!("Searching page {}/{}... ", page, cli.pages);
        } else {
            eprint!("Searching... ");
        }
        let html = search(&client, &base_url, &query, page)?;
        let results = parse_results(&html, cli.limit.saturating_sub(all_results.len()));
        let count = results.len();
        all_results.extend(results);
        eprintln!("{} results", count);

        if count == 0 || all_results.len() >= cli.limit {
            break;
        }
    }

    if all_results.is_empty() {
        eprintln!("No results found for: {query}");
        return Ok(());
    }

    let total = all_results.len();
    for (i, book) in all_results.iter_mut().enumerate() {
        if book.detail_url.is_empty() {
            continue;
        }
        eprint!("\rFetching magnet links ({}/{})... ", i + 1, total);
        if let Ok(hash) = fetch_info_hash(&client, &base_url, &book.detail_url) {
            book.magnet = magnet_from_hash(&hash, &book.title);
        }
    }
    if total > 0 {
        eprintln!("done");
    }

    println!();
    display(&all_results);

    Ok(())
}
