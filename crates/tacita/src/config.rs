//! Config file, saved state, and their validation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tacita_core::pairs::{Pair, Pairs, PairsError};

use crate::paths::write_private;

const CONFIG_FILE: &str = "config.toml";
const STATE_FILE: &str = "state.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protection {
    Off,
    Pairs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicCfg {
    pub protection: Protection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterCfg {
    pub protection: Protection,
    #[serde(default)]
    pub only: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step0Cfg {
    #[serde(default)]
    pub pairs: Vec<Pair>,
    pub block_probe: Option<String>,
    pub stream_probe: Option<String>,
    pub guard_probe: Option<String>,
    pub record_dir: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub secret: String,
    pub anthropic: AnthropicCfg,
    pub openrouter: OpenRouterCfg,
    #[serde(default)]
    pub step0: Step0Cfg,
}

impl std::fmt::Debug for Step0Cfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step0Cfg")
            .field("pairs", &format_args!("[{} hidden]", self.pairs.len()))
            .field("block_probe", &self.block_probe)
            .field("stream_probe", &self.stream_probe)
            .field("guard_probe", &self.guard_probe)
            .field("record_dir", &self.record_dir)
            .finish()
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("secret", &"[hidden]")
            .field("anthropic", &self.anthropic)
            .field("openrouter", &self.openrouter)
            .field("step0", &self.step0)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    Openrouter,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrouter_model: Option<String>,
}

/// Display text never contains a real value or the secret. `Pairs` has no
/// `source()` because anyhow would walk it and print PairsError's values;
/// Debug is the same text as Display for the same reason.
#[derive(thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error on {file}: {source}")]
    Io {
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{file} line {line}: {message}")]
    Toml {
        file: String,
        line: usize,
        message: String,
    },
    #[error("secret must be exactly 32 lowercase hex characters")]
    BadSecret,
    #[error("port must not be 0")]
    BadPort,
    #[error("openrouter.protection = \"off\" is refused")]
    OpenRouterUnprotected,
    #[error("{}", pairs_text(.0))]
    Pairs(PairsError),
    #[error("step0.record_dir must be an absolute path")]
    RecordDirNotAbsolute,
    #[error("config.toml already exists; refusing to overwrite it")]
    ConfigExists,
    #[error(
        "state.toml: provider openrouter needs a model; run `tacita use openrouter --model <slug>`"
    )]
    StateNoModel,
    #[error("step0.{0} must not be empty")]
    EmptyProbe(&'static str),
}

impl std::fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

fn pairs_text(e: &PairsError) -> &'static str {
    match e {
        PairsError::Empty => "step0.pairs is invalid: the list is empty",
        PairsError::EmptyValue => "step0.pairs is invalid: a value is empty",
        PairsError::BadStandin(_) => "step0.pairs is invalid: a stand-in is malformed",
        PairsError::DuplicateReal(_) => "step0.pairs is invalid: a real value is duplicated",
        PairsError::DuplicateStandin(_) => "step0.pairs is invalid: a stand-in is duplicated",
        PairsError::Nested(..) => "step0.pairs is invalid: one stand-in contains another",
        PairsError::Overlap(..) => "step0.pairs is invalid: two stand-ins overlap",
        PairsError::Crossed(..) => {
            "step0.pairs is invalid: a real value and a stand-in contain each other"
        }
        PairsError::ReplacementChar(_) => "step0.pairs is invalid: a real value contains U+FFFD",
    }
}

fn io_err(file: &str, source: std::io::Error) -> ConfigError {
    ConfigError::Io {
        file: file.to_string(),
        source,
    }
}

fn toml_err(file: &str, text: &str, e: toml::de::Error) -> ConfigError {
    let line = e
        .span()
        .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1)
        .unwrap_or(0);
    ConfigError::Toml {
        file: file.to_string(),
        line,
        message: e.message().to_string(),
    }
}

impl Config {
    pub fn load(home: &Path) -> Result<Config, ConfigError> {
        let text =
            std::fs::read_to_string(home.join(CONFIG_FILE)).map_err(|e| io_err(CONFIG_FILE, e))?;
        let cfg: Config = toml::from_str(&text).map_err(|e| toml_err(CONFIG_FILE, &text, e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let s = &self.secret;
        if s.len() != 32 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(ConfigError::BadSecret);
        }
        if self.port == 0 {
            return Err(ConfigError::BadPort);
        }
        if self.openrouter.protection == Protection::Off {
            return Err(ConfigError::OpenRouterUnprotected);
        }
        if self.anthropic.protection == Protection::Pairs
            || self.openrouter.protection == Protection::Pairs
        {
            self.pairs()?;
        }
        for (name, v) in [
            ("block_probe", &self.step0.block_probe),
            ("stream_probe", &self.step0.stream_probe),
            ("guard_probe", &self.step0.guard_probe),
        ] {
            if v.as_deref() == Some("") {
                return Err(ConfigError::EmptyProbe(name));
            }
        }
        if let Some(d) = &self.step0.record_dir
            && !d.is_absolute()
        {
            return Err(ConfigError::RecordDirNotAbsolute);
        }
        Ok(())
    }

    /// Creates the private home and a default config with a fresh random secret.
    pub fn create_default(home: &Path) -> Result<Config, ConfigError> {
        use std::io::Read;
        let mut raw = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut raw))
            .map_err(|e| io_err("/dev/urandom", e))?;
        let secret: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        crate::paths::ensure_private_dir(home).map_err(|e| io_err("Tacita home", e))?;
        let text = DEFAULT_CONFIG.replace("{secret}", &secret);
        crate::paths::create_private_new(&home.join(CONFIG_FILE), text.as_bytes()).map_err(
            |e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    ConfigError::ConfigExists
                } else {
                    io_err(CONFIG_FILE, e)
                }
            },
        )?;
        Config::load(home)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.secret)
    }

    pub fn pairs(&self) -> Result<Arc<Pairs>, ConfigError> {
        Pairs::new(self.step0.pairs.clone())
            .map(Arc::new)
            .map_err(ConfigError::Pairs)
    }
}

const DEFAULT_CONFIG: &str = r#"port = 47821
secret = "{secret}"

[anthropic]
protection = "off"     # "off" = Step 0 test 1, "pairs" = test 2

[openrouter]
protection = "pairs"   # "off" is refused
only = []              # optional trusted OpenRouter provider slugs

[step0]
pairs = [
  { real = "acmewidgets", standin = "client_x01" },
  { real = "AcmeWidgets", standin = "ClientX01" },
  { real = "Jeanne Fictive", standin = "person_x01" },
]
block_probe = "TACITA_BLOCK_PROBE"
stream_probe = "TACITA_STREAM_PROBE"
guard_probe = "TACITA_GUARD_PROBE"
# record_dir = "/Users/you/tacita-corpus"   # Step 0 test 5; never inside a git repo
"#;

impl State {
    /// A missing file means Anthropic.
    pub fn load(home: &Path) -> Result<State, ConfigError> {
        match std::fs::read_to_string(home.join(STATE_FILE)) {
            Ok(text) => {
                let s: State = toml::from_str(&text).map_err(|e| toml_err(STATE_FILE, &text, e))?;
                if s.provider == Provider::Openrouter && s.openrouter_model.is_none() {
                    return Err(ConfigError::StateNoModel);
                }
                Ok(s)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State {
                provider: Provider::Anthropic,
                openrouter_model: None,
            }),
            Err(e) => Err(io_err(STATE_FILE, e)),
        }
    }

    pub fn save(&self, home: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string(self).expect("State serialises");
        write_private(&home.join(STATE_FILE), text.as_bytes()).map_err(|e| io_err(STATE_FILE, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home_with(text: &str) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(CONFIG_FILE), text).unwrap();
        t
    }

    fn default_text() -> String {
        DEFAULT_CONFIG.replace("{secret}", &"ab".repeat(16))
    }

    #[test]
    fn default_config_loads() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        let c = Config::create_default(&home).unwrap();
        assert_eq!(c.port, 47821);
        assert_eq!(c.anthropic.protection, Protection::Off);
        assert_eq!(c.step0.pairs.len(), 3);
        assert!(c.base_url().starts_with("http://127.0.0.1:47821/"));
        assert!(c.pairs().is_ok());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join(CONFIG_FILE)), 0o600);
        // two defaults have different secrets
        let c2 = Config::create_default(&t.path().join("h2")).unwrap();
        assert_ne!(c.secret, c2.secret);
    }

    #[test]
    fn openrouter_off_is_refused() {
        let text = default_text().replace("protection = \"pairs\"   #", "protection = \"off\"   #");
        let t = home_with(&text);
        assert!(matches!(
            Config::load(t.path()),
            Err(ConfigError::OpenRouterUnprotected)
        ));
    }

    #[test]
    fn bad_secret_is_refused() {
        for bad in [
            "short",
            &"AB".repeat(16),
            &"zz".repeat(16),
            &"ab".repeat(17),
        ] {
            let text = default_text().replace(&"ab".repeat(16), bad);
            let t = home_with(&text);
            assert!(
                matches!(Config::load(t.path()), Err(ConfigError::BadSecret)),
                "{bad}"
            );
        }
    }

    #[test]
    fn port_zero_is_refused() {
        let t = home_with(&default_text().replace("47821", "0"));
        assert!(matches!(Config::load(t.path()), Err(ConfigError::BadPort)));
    }

    #[test]
    fn invalid_pairs_are_refused_without_leaking_values() {
        let text = default_text().replace("\"client_x01\"", "\"person_x01\"");
        let t = home_with(&text);
        let err = Config::load(t.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Pairs(_)));
        let shown = err.to_string();
        for leak in [
            "acmewidgets",
            "AcmeWidgets",
            "Jeanne",
            "person_x01",
            "client_x01",
        ] {
            assert!(!shown.contains(leak), "{shown}");
        }
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn toml_error_has_line_and_no_snippet() {
        let secret_line = format!("secret = \"{}\"\nport = oops\n", "ab".repeat(16));
        let t = home_with(&secret_line);
        let shown = Config::load(t.path()).unwrap_err().to_string();
        assert!(shown.contains("config.toml line 2"), "{shown}");
        assert!(!shown.contains("abab"), "{shown}");
    }

    #[test]
    fn unknown_fields_are_refused() {
        let t = home_with(&default_text().replace("block_probe", "blok_probe"));
        let err = Config::load(t.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "{err}");
        let t = home_with(&(default_text() + "extra = 1\n"));
        assert!(Config::load(t.path()).is_err());
    }

    #[test]
    fn empty_probe_is_refused() {
        for name in ["block_probe", "stream_probe", "guard_probe"] {
            let text = default_text().replace(
                &format!(
                    "{name} = \"TACITA_{}_PROBE\"",
                    name.split('_').next().unwrap().to_uppercase()
                ),
                &format!("{name} = \"\""),
            );
            let t = home_with(&text);
            assert!(
                matches!(Config::load(t.path()), Err(ConfigError::EmptyProbe(n)) if n == name),
                "{name}"
            );
        }
    }

    #[test]
    fn record_dir_must_be_absolute() {
        let text = default_text().replace(
            "# record_dir = \"/Users/you/tacita-corpus\"",
            "record_dir = \"corpus\"",
        );
        let t = home_with(&text);
        assert!(matches!(
            Config::load(t.path()),
            Err(ConfigError::RecordDirNotAbsolute)
        ));
        let ok = default_text().replace("# record_dir = \"/Users/you", "record_dir = \"/Users/you");
        let t = home_with(&ok);
        assert!(Config::load(t.path()).is_ok());
    }

    #[test]
    fn unknown_fields_are_refused_everywhere() {
        let cases: Vec<(&str, String)> = vec![
            ("top-level", format!("extra = 1\n{}", default_text())),
            (
                "anthropic",
                default_text().replace("[anthropic]\n", "[anthropic]\nextra = 1\n"),
            ),
            (
                "openrouter",
                default_text().replace("[openrouter]\n", "[openrouter]\nextra = 1\n"),
            ),
            (
                "pair",
                default_text().replace(
                    "{ real = \"acmewidgets\", standin = \"client_x01\" }",
                    "{ real = \"acmewidgets\", standin = \"client_x01\", note = \"x\" }",
                ),
            ),
        ];
        for (name, text) in cases {
            let t = home_with(&text);
            let e = Config::load(t.path()).unwrap_err();
            assert!(matches!(e, ConfigError::Toml { .. }), "{name}: {e}");
            assert!(e.to_string().contains("unknown field"), "{name}: {e}");
        }
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join(STATE_FILE),
            "provider = \"anthropic\"\nextra = 1\n",
        )
        .unwrap();
        assert!(matches!(
            State::load(t.path()),
            Err(ConfigError::Toml { .. })
        ));
    }

    #[test]
    fn debug_and_anyhow_hide_secrets_and_real_values() {
        let secret = "ab".repeat(16);
        let hidden = |s: &str| {
            for leak in [
                secret.as_str(),
                "acmewidgets",
                "AcmeWidgets",
                "Jeanne",
                "person_x01",
                "client_x01",
            ] {
                assert!(!s.contains(leak), "{leak} in {s}");
            }
        };
        let t = home_with(&default_text());
        let cfg = Config::load(t.path()).unwrap();
        let d = format!("{cfg:?}");
        hidden(&d);
        assert!(d.contains("[hidden]"), "{d}");
        hidden(&format!("{:?}", cfg.step0));
        let bad = default_text().replace("\"client_x01\"", "\"person_x01\"");
        let t = home_with(&bad);
        let err = Config::load(t.path()).unwrap_err();
        assert!(std::error::Error::source(&err).is_none());
        let shown = err.to_string();
        assert_eq!(format!("{err:?}"), shown);
        hidden(&shown);
        let any = anyhow::Error::from(err);
        hidden(&format!("{any:#}"));
        hidden(&format!("{any:?}"));
    }

    #[test]
    fn create_default_never_overwrites() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(CONFIG_FILE), "mine").unwrap();
        let e = Config::create_default(t.path()).unwrap_err();
        assert!(matches!(e, ConfigError::ConfigExists), "{e}");
        assert_eq!(
            std::fs::read_to_string(t.path().join(CONFIG_FILE)).unwrap(),
            "mine"
        );
        let names: Vec<_> = std::fs::read_dir(t.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "temp file left behind");
    }

    #[test]
    fn state_openrouter_needs_a_model() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(STATE_FILE), "provider = \"openrouter\"\n").unwrap();
        let e = State::load(t.path()).unwrap_err();
        assert!(matches!(e, ConfigError::StateNoModel));
        assert_eq!(
            e.to_string(),
            "state.toml: provider openrouter needs a model; run `tacita use openrouter --model <slug>`"
        );
    }

    #[test]
    fn state_defaults_to_anthropic() {
        let t = tempfile::tempdir().unwrap();
        let s = State::load(t.path()).unwrap();
        assert_eq!(s.provider, Provider::Anthropic);
        assert!(s.openrouter_model.is_none());
    }

    #[test]
    fn state_round_trips() {
        let t = tempfile::tempdir().unwrap();
        State {
            provider: Provider::Openrouter,
            openrouter_model: Some("x/y".into()),
        }
        .save(t.path())
        .unwrap();
        let s = State::load(t.path()).unwrap();
        assert_eq!(s.provider, Provider::Openrouter);
        assert_eq!(s.openrouter_model.as_deref(), Some("x/y"));
    }

    #[test]
    fn state_file_is_0600() {
        let t = tempfile::tempdir().unwrap();
        State {
            provider: Provider::Anthropic,
            openrouter_model: None,
        }
        .save(t.path())
        .unwrap();
        let mode = std::fs::metadata(t.path().join(STATE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
