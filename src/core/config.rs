use std::{
    env, fmt,
    num::NonZeroU64,
    path::PathBuf,
    sync::OnceLock,
};

use eyre::{Context, ContextCompat, Result};
use rosu_v2::prelude::Grade;
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

static CONFIG: OnceLock<BotConfig> = OnceLock::new();

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
        self.folders.join("server_settings.json")
    }

    pub fn danser(&self) -> &PathBuf {
        &self.danser
    }

    pub fn folders(&self) -> &PathBuf {
        &self.folders
    }

    pub fn downloads(&self) -> PathBuf {
        self.folders.join("Downloads")
    }

    pub fn replays(&self) -> PathBuf {
        self.folders.join("Replays")
    }

    /// Avoid this, use Context::skin_list instead if possible
    pub fn skins(&self) -> PathBuf {
        self.folders.join("Skins")
    }

    pub fn songs(&self) -> PathBuf {
        self.folders.join("Songs")
    }
}

pub struct Tokens {
    pub discord: String,
    pub osu_client_id: u64,
    pub osu_client_secret: String,
    pub upload_secret: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("discord", &"[REDACTED]")
            .field("osu_client_id", &self.osu_client_id)
            .field("osu_client_secret", &"[REDACTED]")
            .field("upload_secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug)]
pub struct Emojis {
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

impl Emojis {
    /// Return the configured Discord emoji string for `grade`.
    pub fn grade_emoji(&self, grade: Grade) -> &str {
        match grade {
            Grade::XH => &self.grade_xh,
            Grade::X => &self.grade_x,
            Grade::SH => &self.grade_sh,
            Grade::S => &self.grade_s,
            Grade::A => &self.grade_a,
            Grade::B => &self.grade_b,
            Grade::C => &self.grade_c,
            Grade::D => &self.grade_d,
            _ => &self.grade_fail,
        }
    }
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
                upload_secret: env_var("UPLOAD_SECRET")?,
            },
            paths: Paths {
                danser: env_var("DANSER_PATH")?,
                folders: env_var("FOLDERS_PATH")?,
            },
            emojis: Emojis {
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
    u64: s => { s.trim().parse().ok() },
    PathBuf: s => { s.parse().ok() },
    String: s => { Some(s.to_owned()) },
    Id<GuildMarker>: s => { s.trim().parse::<NonZeroU64>().ok().map(Id::from) },
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
