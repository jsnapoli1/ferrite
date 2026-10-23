use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration, loaded from `ferrite.toml` and/or environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address to listen on.
    pub listen: String,
    /// Where the database, blobs and scratch space live.
    pub data_dir: PathBuf,
    /// Public base URL (used for share links and MCP metadata).
    pub public_url: String,
    /// Allow anyone to sign up (otherwise admins create accounts).
    pub open_registration: bool,
    /// Default quota for new users, in bytes (None = unlimited).
    pub default_quota: Option<i64>,
    /// Maximum upload size in bytes.
    pub max_upload: u64,
    /// Days to keep trashed files before purging.
    pub trash_retention_days: i64,
    /// Maximum versions kept per file.
    pub max_versions: i64,
    /// ICE servers handed to browsers for calls. On a LAN, none are needed.
    pub ice_servers: Vec<IceServer>,
    /// Craft app integration (desktop apps of the ArtCraft suite).
    pub craft: CraftConfig,
    /// Cookies are marked `Secure` when true (set when served over HTTPS).
    pub secure_cookies: bool,
    /// Built-in SFU for Talk calls.
    pub sfu: SfuSettings,
    /// Video quality ceiling for calls.
    pub calls: CallSettings,
    /// Outgoing mail for the server's own messages (invitations, security notices).
    pub smtp: Option<crate::mailer::SmtpSettings>,
    /// Every account must use two-factor authentication: users without it set it up at
    /// their next password sign-in, before they get a session.
    pub require_two_factor: bool,
    /// OpenID Connect single sign-on providers (`[[oidc]]` tables).
    pub oidc: Vec<crate::sso::OidcProvider>,
    /// Web Push (browser and installed-app notifications).
    pub push: crate::push::PushConfig,
}

/// Settings of the built-in Selective Forwarding Unit (calls beyond a handful of people).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SfuSettings {
    pub enabled: bool,
    /// UDP address for call media. Open this port in your firewall.
    pub udp_bind: String,
    /// IP addresses advertised to browsers (default: this machine's primary IP and loopback).
    pub advertise: Vec<String>,
}

impl Default for SfuSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            udp_bind: "0.0.0.0:10000".into(),
            advertise: Vec::new(),
        }
    }
}

/// The highest video quality calls may use. Everything below the ceiling adapts on its own
/// (simulcast layers, bandwidth estimation), so this sets what capable devices and links can
/// reach; see the README's "Call quality" section for what each step costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CallSettings {
    /// Capture and top simulcast layer height: "360p", "480p", "720p", "1080p", "1440p" or "2160p".
    pub max_resolution: String,
    /// Frames per second, 15 to 60.
    pub max_fps: u32,
}

impl Default for CallSettings {
    fn default() -> Self {
        Self {
            max_resolution: "720p".into(),
            max_fps: 30,
        }
    }
}

impl CallSettings {
    /// (height, fps) after validation.
    pub fn quality(&self) -> anyhow::Result<(u32, u32)> {
        let height = match self.max_resolution.trim().to_ascii_lowercase().as_str() {
            "360p" => 360,
            "480p" => 480,
            "720p" => 720,
            "1080p" => 1080,
            "1440p" => 1440,
            "2160p" | "4k" => 2160,
            other => anyhow::bail!("calls.max_resolution must be 360p, 480p, 720p, 1080p, 1440p or 2160p, not {other:?}"),
        };
        if !(15..=60).contains(&self.max_fps) {
            anyhow::bail!("calls.max_fps must be between 15 and 60, not {}", self.max_fps);
        }
        Ok((height, self.max_fps))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CraftConfig {
    /// Directory containing `<app>-cli` binaries; when set, Ferrite can drive the apps headlessly
    /// through their MCP servers. Each app is also looked up on `PATH`.
    pub bin_dir: Option<PathBuf>,
    /// Per-app overrides of the command used to start an app's MCP server.
    pub commands: std::collections::BTreeMap<String, CraftCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraftCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            data_dir: PathBuf::from("./ferrite-data"),
            public_url: "http://127.0.0.1:8080".into(),
            open_registration: false,
            default_quota: None,
            max_upload: 16 * 1024 * 1024 * 1024,
            trash_retention_days: 30,
            max_versions: 50,
            ice_servers: Vec::new(),
            craft: CraftConfig::default(),
            secure_cookies: false,
            sfu: SfuSettings::default(),
            calls: CallSettings::default(),
            smtp: None,
            require_two_factor: false,
            oidc: Vec::new(),
            push: crate::push::PushConfig::default(),
        }
    }
}

impl Config {
    /// Loads `path` if it exists, then applies `FERRITE_*` environment overrides.
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut cfg: Config = match path {
            Some(p) if p.exists() => toml::from_str(&std::fs::read_to_string(p)?)?,
            _ => Config::default(),
        };
        if let Ok(v) = std::env::var("FERRITE_LISTEN") {
            cfg.listen = v;
        }
        if let Ok(v) = std::env::var("FERRITE_DATA_DIR") {
            cfg.data_dir = v.into();
        }
        if let Ok(v) = std::env::var("FERRITE_PUBLIC_URL") {
            cfg.public_url = v;
        }
        if let Ok(v) = std::env::var("FERRITE_OPEN_REGISTRATION") {
            cfg.open_registration = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Ok(v) = std::env::var("FERRITE_SFU_UDP") {
            cfg.sfu.udp_bind = v;
        }
        if let Ok(v) = std::env::var("FERRITE_SFU") {
            cfg.sfu.enabled = !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"));
        }
        if let Ok(v) = std::env::var("FERRITE_CRAFT_BIN_DIR") {
            cfg.craft.bin_dir = Some(v.into());
        }
        if let Ok(v) = std::env::var("FERRITE_REQUIRE_2FA") {
            cfg.require_two_factor = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Ok(v) = std::env::var("FERRITE_PUSH_ALLOW_INSECURE") {
            cfg.push.allow_insecure_endpoints = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Ok(v) = std::env::var("FERRITE_CALL_RESOLUTION") {
            cfg.calls.max_resolution = v;
        }
        if let Ok(v) = std::env::var("FERRITE_CALL_FPS") {
            cfg.calls.max_fps = v.parse().map_err(|_| anyhow::anyhow!("FERRITE_CALL_FPS must be a number"))?;
        }
        cfg.calls.quality()?;
        cfg.public_url = cfg.public_url.trim_end_matches('/').to_string();
        for (i, p) in cfg.oidc.iter().enumerate() {
            p.validate()?;
            if cfg.oidc[..i].iter().any(|q| q.id == p.id) {
                anyhow::bail!("duplicate [[oidc]] provider id {:?}", p.id);
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::CallSettings;

    fn q(res: &str, fps: u32) -> anyhow::Result<(u32, u32)> {
        CallSettings {
            max_resolution: res.into(),
            max_fps: fps,
        }
        .quality()
    }

    #[test]
    fn call_quality() {
        assert_eq!(CallSettings::default().quality().unwrap(), (720, 30));
        assert_eq!(q("1080p", 60).unwrap(), (1080, 60));
        assert_eq!(q(" 4K ", 30).unwrap(), (2160, 30));
        assert!(q("900p", 30).is_err());
        assert!(q("720p", 61).is_err());
        assert!(q("720p", 10).is_err());
    }
}
