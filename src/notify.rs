//! Discord webhook notifications (P3 G6).
//!
//! The message formatters + the webhook-URL validator are pure and unit-tested.
//! [`DiscordNotifier`] is the thin HTTPS `POST` shell over `ureq` (blocking,
//! rustls, no async runtime). It is **best-effort + non-blocking**: every post
//! is handed to a detached thread, so a slow / dead / rate-limited Discord can
//! never stall or crash mining.

/// A Discord `content` line announcing a solved block (the unambiguous "solution"
/// signal, fired on a solo `/work/submit` accept). Contains the height, the block
/// hash, and the worker so the channel is self-describing.
pub fn block_found_message(height: u64, block_hash: &str, worker: &str) -> String {
    format!("\u{1F389} CSD block found! height {height} \u{00B7} {block_hash} \u{00B7} worker {worker}")
}

/// A Discord `content` line for an accepted-share milestone (the pool-side
/// "solution" proxy — a client pool miner never learns it found a block, so the
/// closest signal is accepted shares). Contains the running total + the worker.
pub fn share_accepted_message(accepted_total: u64, worker: &str) -> String {
    format!("\u{2705} {accepted_total} shares accepted \u{00B7} worker {worker}")
}

/// The JSON body a Discord webhook expects: `{"content": "<msg>"}`.
pub fn discord_payload(content: &str) -> serde_json::Value {
    serde_json::json!({ "content": content })
}

/// A best-effort Discord webhook notifier. [`post`](Self::post) NEVER blocks the
/// caller or panics: it hands the HTTPS POST to a DETACHED thread (never joined),
/// so a hung / dead / 429-ing Discord cannot stall or crash the mining loop.
/// Transport + HTTP errors are swallowed and logged at WARN.
pub struct DiscordNotifier {
    webhook: String,
    solutions_only: bool,
}

impl DiscordNotifier {
    /// Build a notifier. `webhook` should already be [`validate_webhook_url`]-ed.
    /// `solutions_only` = fire ONLY on solved blocks (skip share milestones).
    pub fn new(webhook: String, solutions_only: bool) -> Self {
        DiscordNotifier {
            webhook,
            solutions_only,
        }
    }

    /// Whether this notifier fires only on solved blocks (not share milestones).
    pub fn solutions_only(&self) -> bool {
        self.solutions_only
    }

    /// Fire-and-forget: POST `{"content": content}` to the webhook from a
    /// detached thread. Returns immediately; connect/read are bounded by short
    /// timeouts and any error is swallowed + logged — a webhook hiccup must
    /// never affect mining.
    pub fn post(&self, content: String) {
        let webhook = self.webhook.clone();
        // Detached on purpose — the mining loop never joins it.
        let _ = std::thread::Builder::new()
            .name("discord-notify".to_string())
            .spawn(move || {
                let agent = ureq::AgentBuilder::new()
                    .timeout_connect(std::time::Duration::from_secs(5))
                    .timeout_read(std::time::Duration::from_secs(10))
                    .build();
                match agent.post(&webhook).send_json(discord_payload(&content)) {
                    Ok(_) => {}
                    Err(e) => tracing::warn!("discord webhook post failed (ignored): {e}"),
                }
            });
    }
}

/// Validate a Discord webhook URL before we ever POST to it.
///
/// Accepts ONLY `https://` URLs whose host is a Discord endpoint
/// (`discord.com`, `discordapp.com`, `canary.discord.com`, or any `*.discord.com`
/// subdomain — all controlled by Discord). Rejects plaintext `http://` (a token
/// in a webhook URL must never travel in the clear) and any non-Discord host.
///
/// The host check is **spoof-resistant**: the host is the segment between
/// `https://` and the next `/`, `?`, or `#`; a `@` (userinfo, e.g.
/// `https://discord.com@evil.com`) is rejected; the `*.discord.com` match uses a
/// leading dot so `evildiscord.com` does NOT match.
pub fn validate_webhook_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("webhook must be https:// (got {url:?})"))?;

    // Host = up to the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Reject embedded credentials — `https://discord.com@evil.com/...` has
    // authority `discord.com@evil.com`; the REAL host is `evil.com`.
    if authority.contains('@') {
        return Err(format!(
            "webhook host must not contain credentials: {authority:?}"
        ));
    }
    // Drop an optional :port, lowercase for comparison.
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();

    const ALLOWED: [&str; 3] = ["discord.com", "discordapp.com", "canary.discord.com"];
    if ALLOWED.contains(&host.as_str()) || host.ends_with(".discord.com") {
        Ok(())
    } else {
        Err(format!("webhook host is not a Discord endpoint: {host:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_found_message_contains_height_hash_worker() {
        let m = block_found_message(12345, "00ab_c0ffee", "csd1rig");
        assert!(m.contains("12345"), "{m}");
        assert!(m.contains("00ab_c0ffee"), "{m}");
        assert!(m.contains("csd1rig"), "{m}");
    }

    #[test]
    fn share_accepted_message_contains_count_and_worker() {
        let m = share_accepted_message(100, "csd1rig");
        assert!(m.contains("100"), "{m}");
        assert!(m.contains("csd1rig"), "{m}");
    }

    #[test]
    fn discord_payload_wraps_content() {
        assert_eq!(discord_payload("hi")["content"], "hi");
    }

    #[test]
    fn validate_webhook_url_accepts_discord_https() {
        assert!(validate_webhook_url("https://discord.com/api/webhooks/123/abc").is_ok());
        assert!(validate_webhook_url("https://discordapp.com/api/webhooks/1/x").is_ok());
        assert!(validate_webhook_url("https://canary.discord.com/api/webhooks/1/x").is_ok());
        assert!(validate_webhook_url("https://ptb.discord.com/api/webhooks/1/x").is_ok()); // *.discord.com
    }

    #[test]
    fn validate_webhook_url_rejects_non_https_and_non_discord() {
        // Not https (plaintext would leak the token).
        assert!(validate_webhook_url("http://discord.com/api/webhooks/1/x").is_err());
        // Wrong host.
        assert!(validate_webhook_url("https://evil.com/api/webhooks").is_err());
        // Path-spoof: real host is evil.com.
        assert!(validate_webhook_url("https://evil.com/discord.com").is_err());
        // Userinfo-spoof: real host is evil.com.
        assert!(validate_webhook_url("https://discord.com@evil.com/api").is_err());
        // Suffix-spoof: evildiscord.com is NOT a *.discord.com subdomain.
        assert!(validate_webhook_url("https://evildiscord.com/api").is_err());
        // Empty / garbage.
        assert!(validate_webhook_url("not a url").is_err());
    }

    #[test]
    fn discord_notifier_post_is_nonblocking_and_swallows_errors() {
        // Pointed at a refused address: post() must return immediately (the work
        // is detached) and never panic — proving a dead Discord can't stall
        // mining. If post() blocked, this test would hang.
        let n = DiscordNotifier::new("https://127.0.0.1:1".to_string(), false);
        n.post("hello".to_string());
        n.post("world".to_string());
        assert!(!n.solutions_only());
    }

    #[test]
    fn discord_notifier_solutions_only_getter() {
        assert!(DiscordNotifier::new("https://discord.com/x".to_string(), true).solutions_only());
        assert!(!DiscordNotifier::new("https://discord.com/x".to_string(), false).solutions_only());
    }
}
