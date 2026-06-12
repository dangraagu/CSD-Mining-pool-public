//! Discord webhook notifications (P3 G6) — **pure cores only**.
//!
//! This module holds the message formatters + the webhook-URL validator, which
//! are pure and fully unit-tested. The actual HTTPS `POST` shell
//! (`DiscordNotifier`) needs a TLS-capable HTTP client (`ureq`), a dependency
//! deliberately deferred until the operator/security review signs off on adding
//! TLS to this previously TLS-free binary — so it lands separately.

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
}
