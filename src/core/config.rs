use std::{env, fmt, num::NonZeroU64, path::PathBuf};

use eyre::{Context, ContextCompat, Result};
use once_cell::sync::OnceCell;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, UserMarker},
    Id,
};

static CONFIG: OnceCell<BotConfig> = OnceCell::new();

#[derive(Debug)]
pub struct BotConfig {
    pub database_url: String,
    pub tokens: Tokens,
    pub paths: Paths,
    pub emojis: Emojis,
    pub owners: Vec<Id<UserMarker>>,
    pub dev_guild: Id<GuildMarker>,
    pub upload_url: String,
}

#[derive(Debug)]
pub struct Paths {
    danser: PathBuf,
    folders: PathBuf,
}

impl Paths {
    pub fn server_settings(&self) -> PathBuf {
        let mut path = self.folders.clone();
        path.push("server_settings.json");

        path
    }

    pub fn danser(&self) -> &PathBuf {
        &self.danser
    }

    pub fn folders(&self) -> &PathBuf {
        &self.folders
    }

    pub fn downloads(&self) -> PathBuf {
        let mut path = self.folders.clone();
        path.push("Downloads");

        path
    }

    pub fn replays(&self) -> PathBuf {
        let mut path = self.folders.clone();
        path.push("Replays");

        path
    }

    /// Avoid this, use Context::skin_list instead if possible
    pub fn skins(&self) -> PathBuf {
        let mut path = self.folders.clone();
        path.push("Skins");

        path
    }

    pub fn songs(&self) -> PathBuf {
        let mut path = self.folders.clone();
        path.push("Songs");

        path
    }
}

pub struct Tokens {
    pub discord: String,
    pub osu_client_id: u64,
    pub osu_client_secret: String,
    #[allow(dead_code)]
    pub osu_api_key: String,
    pub upload_secret: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("discord", &"[REDACTED]")
            .field("osu_client_id", &self.osu_client_id)
            .field("osu_client_secret", &"[REDACTED]")
            .field("osu_api_key", &"[REDACTED]")
            .field("upload_secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug)]
pub struct Emojis {
    #[allow(dead_code)]
    pub man_running: String,
    #[allow(dead_code)]
    pub white_check_mark: String,
    #[allow(dead_code)]
    pub hourglass: String,
    pub hit_300: String,
    pub hit_100: String,
    pub hit_50: String,
    pub hit_miss: String,
    pub grade_xh: String,
    pub grade_x: String,
    pub grade_sh: String,
    pub grade_s: String,
    pub grade_a: String,
    pub grade_b: String,
    pub grade_c: String,
    pub grade_d: String,
    pub grade_fail: String,
}

impl BotConfig {
    pub fn try_get() -> Option<&'static Self> {
        CONFIG.get()
    }

    pub fn get() -> &'static Self {
        CONFIG
            .get()
            .expect("`BotConfig::init` must be called first")
    }

    pub fn init() -> Result<()> {
        let dev_guild = if cfg!(debug_assertions) {
            env_var("DEV_GUILD_ID")?
        } else {
            env_var("DEV_GUILD_ID").unwrap_or_else(|_| Id::new(1))
        };

        let config = BotConfig {
            database_url: env_var("DATABASE_URL")?,
            tokens: Tokens {
                discord: env_var("DISCORD_TOKEN")?,
                osu_client_id: env_var("OSU_CLIENT_ID")?,
                osu_client_secret: env_var("OSU_CLIENT_SECRET")?,
                osu_api_key: env::var("OSU_API_KEY").unwrap_or_default(),
                upload_secret: env_var("UPLOAD_SECRET")?,
            },
            paths: Paths {
                danser: env_var("DANSER_PATH")?,
                folders: env_var("FOLDERS_PATH")?,
            },
            emojis: Emojis {
                man_running: env::var("MAN_RUNNING").unwrap_or_default(),
                white_check_mark: env::var("WHITE_CHECK_MARK").unwrap_or_default(),
                hourglass: env::var("HOURGLASS").unwrap_or_default(),
                hit_300: env_var("EMOJI_HIT_300")?,
                hit_100: env_var("EMOJI_HIT_100")?,
                hit_50: env_var("EMOJI_HIT_50")?,
                hit_miss: env_var("EMOJI_HIT_MISS")?,
                grade_xh: env_var("GRADE_XH")?,
                grade_x: env_var("GRADE_X")?,
                grade_sh: env_var("GRADE_SH")?,
                grade_s: env_var("GRADE_S")?,
                grade_a: env_var("GRADE_A")?,
                grade_b: env_var("GRADE_B")?,
                grade_c: env_var("GRADE_C")?,
                grade_d: env_var("GRADE_D")?,
                grade_fail: env_var("GRADE_FAIL")?,
            },
            owners: env_var("OWNERS_USER_ID")?,
            dev_guild,
            upload_url: env_var("UPLOAD_URL")?,
        };

        if CONFIG.set(config).is_err() {
            error!("CONFIG was already set");
        }

        Ok(())
    }
}

trait EnvKind: Sized {
    const EXPECTED: &'static str;

    fn from_str(s: &str) -> Option<Self>;
}

macro_rules! env_kind {
    ($($ty:ty: $arg:ident => $impl:block,)*) => {
        $(
            impl EnvKind for $ty {
                const EXPECTED: &'static str = stringify!($ty);

                fn from_str($arg: &str) -> Option<Self> {
                    $impl
                }
            }
        )*
    };
}

env_kind! {
    u16: s => { s.trim().parse().ok() },
    u64: s => { s.trim().parse().ok() },
    PathBuf: s => { s.parse().ok() },
    String: s => { Some(s.to_owned()) },
    Id<UserMarker>: s => { s.trim().parse::<NonZeroU64>().ok().map(Id::from) },
    Id<GuildMarker>: s => { s.trim().parse::<NonZeroU64>().ok().map(Id::from) },
    Id<ChannelMarker>: s => { s.trim().parse::<NonZeroU64>().ok().map(Id::from) },
    Vec<Id<UserMarker>>: s => {
        s.split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| part.parse::<NonZeroU64>().map(Id::from))
            .collect::<Result<Vec<_>, _>>()
            .ok()
    },
}

fn is_sensitive_env(name: &str) -> bool {
    name.contains("TOKEN")
        || name.contains("SECRET")
        || name.contains("KEY")
        || name == "DATABASE_URL"
}

fn env_var<T: EnvKind>(name: &'static str) -> Result<T> {
    let value = env::var(name).with_context(|| format!("missing env variable `{name}`"))?;

    T::from_str(&value).with_context(|| {
        let rendered_val = if is_sensitive_env(name) {
            "[REDACTED]"
        } else {
            &value
        };
        format!(
            "failed to parse env variable `{name}={rendered_val}`; expected {expected}",
            expected = T::EXPECTED
        )
    })
}
