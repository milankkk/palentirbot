use std::collections::{BTreeMap, HashSet};

use flurry::HashMap as FlurryMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};

use crate::util::hasher::IntBuildHasher;

type Servers = FlurryMap<Id<GuildMarker>, Server, IntBuildHasher>;

#[derive(Debug, Deserialize, Serialize)]
pub struct RootSettings {
    #[serde(rename = "Servers", with = "servers")]
    pub servers: Servers,
}

#[derive(Clone, Debug, Default)]
pub struct Server {
    pub input_channels: HashSet<Id<ChannelMarker>, IntBuildHasher>,
    pub prefix: Option<String>,
    pub ai_channel: Option<Id<ChannelMarker>>,
}

impl Server {
    /// Returns the effective prefix, defaulting to "!".
    pub fn effective_prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or("!")
    }
}

mod servers {
    use std::{
        collections::HashSet,
        fmt::{Formatter, Result as FmtResult},
    };

    use serde::{
        de::{SeqAccess, Visitor},
        ser::{SerializeSeq, SerializeStruct},
        Deserialize, Deserializer, Serialize, Serializer,
    };
    use twilight_model::id::{
        marker::{ChannelMarker, GuildMarker},
        Id,
    };

    use crate::util::hasher::IntBuildHasher;

    use super::{FlurryMap, Server, Servers};

    #[derive(Deserialize)]
    struct RawServer {
        server_id: Id<GuildMarker>,
        input_channels: HashSet<Id<ChannelMarker>, IntBuildHasher>,
        #[serde(default)]
        prefix: Option<String>,
        #[serde(default)]
        ai_channel: Option<Id<ChannelMarker>>,
    }

    struct ServersVisitor;

    impl<'de> Visitor<'de> for ServersVisitor {
        type Value = Servers;

        fn expecting(&self, f: &mut Formatter<'_>) -> FmtResult {
            f.write_str("a list of servers")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let servers =
                FlurryMap::with_capacity_and_hasher(seq.size_hint().unwrap_or(0), IntBuildHasher);

            {
                let guard = servers.pin();

                while let Some(raw) = seq.next_element()? {
                    let RawServer {
                        server_id,
                        input_channels,
                        prefix,
                        ai_channel,
                    } = raw;

                    let server = Server {
                        input_channels,
                        prefix,
                        ai_channel,
                    };

                    guard.insert(server_id, server);
                }
            }

            Ok(servers)
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Servers, D::Error> {
        d.deserialize_seq(ServersVisitor)
    }

    struct BorrowedRawServer<'s> {
        server_id: Id<GuildMarker>,
        server: &'s Server,
    }

    impl Serialize for BorrowedRawServer<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let mut raw = s.serialize_struct("RawServer", 4)?;

            raw.serialize_field("server_id", &self.server_id)?;
            raw.serialize_field("input_channels", &self.server.input_channels)?;
            raw.serialize_field("prefix", &self.server.prefix)?;
            raw.serialize_field("ai_channel", &self.server.ai_channel)?;
            raw.end()
        }
    }

    pub(super) fn serialize<S: Serializer>(servers: &Servers, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(servers.len()))?;

        for (&server_id, server) in servers.pin().iter() {
            let server = BorrowedRawServer { server_id, server };
            seq.serialize_element(&server)?;
        }

        seq.end()
    }

    struct OverrideServers<'a> {
        servers: &'a Servers,
        override_id: Id<GuildMarker>,
        override_server: &'a Server,
    }

    impl Serialize for OverrideServers<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let guard = self.servers.pin();
            let extra = usize::from(!guard.contains_key(&self.override_id));
            let mut seq = s.serialize_seq(Some(guard.len() + extra))?;

            for (&server_id, server) in guard.iter() {
                let effective = if server_id == self.override_id {
                    self.override_server
                } else {
                    server
                };
                seq.serialize_element(&BorrowedRawServer {
                    server_id,
                    server: effective,
                })?;
            }

            if extra == 1 {
                seq.serialize_element(&BorrowedRawServer {
                    server_id: self.override_id,
                    server: self.override_server,
                })?;
            }

            seq.end()
        }
    }

    pub(crate) fn serialize_with_override(
        servers: &Servers,
        override_id: Id<GuildMarker>,
        override_server: &Server,
    ) -> serde_json::Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Wrapper<'a> {
            #[serde(rename = "Servers")]
            servers: OverrideServers<'a>,
        }

        serde_json::to_vec(&Wrapper {
            servers: OverrideServers {
                servers,
                override_id,
                override_server,
            },
        })
    }
}

pub(crate) fn serialize_root_settings_with_override(
    servers: &Servers,
    override_id: Id<GuildMarker>,
    override_server: &Server,
) -> serde_json::Result<Vec<u8>> {
    servers::serialize_with_override(servers, override_id, override_server)
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DanserSettings {
    pub general: General,
    pub graphics: Graphics,
    pub audio: Audio,
    pub input: Input,
    pub gameplay: Gameplay,
    pub skin: Skin,
    pub cursor: Cursor,
    pub objects: Objects,
    pub playfield: Playfield,
    pub cursor_dance: CursorDance,
    pub knockout: Knockout,
    pub recording: Recording,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn clamp_finite(val: &mut f64, min: f64, max: f64) {
    if !val.is_finite() {
        *val = min;
    } else {
        *val = val.clamp(min, max);
    }
}

pub const RECORDING_720P_WIDTH: i32 = 1280;
pub const RECORDING_720P_HEIGHT: i32 = 720;
pub const RECORDING_900P_WIDTH: i32 = 1600;
pub const RECORDING_900P_HEIGHT: i32 = 900;
pub const RECORDING_1080P_WIDTH: i32 = 1920;
pub const RECORDING_1080P_HEIGHT: i32 = 1080;
pub const LOCKED_RECORDING_FPS: i32 = 60;
pub const DEFAULT_PROFILE_NAME: &str = "Default Profile";

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RenderQuality {
    Hd720p60,
    HdPlus900p60,
    FullHd1080p60,
}

impl RenderQuality {
    pub const fn dimensions(self) -> (i32, i32, i32) {
        match self {
            Self::Hd720p60 => (RECORDING_720P_WIDTH, RECORDING_720P_HEIGHT, LOCKED_RECORDING_FPS),
            Self::HdPlus900p60 => (RECORDING_900P_WIDTH, RECORDING_900P_HEIGHT, LOCKED_RECORDING_FPS),
            Self::FullHd1080p60 => (
                RECORDING_1080P_WIDTH,
                RECORDING_1080P_HEIGHT,
                LOCKED_RECORDING_FPS,
            ),
        }
    }

    pub const fn code(self) -> &'static str {
        match self {
            Self::Hd720p60 => "720p",
            Self::HdPlus900p60 => "900p",
            Self::FullHd1080p60 => "1080p",
        }
    }

    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Hd720p60 => "720p60 (1280x720)",
            Self::HdPlus900p60 => "900p60 (1600x900)",
            Self::FullHd1080p60 => "1080p60 (1920x1080)",
        }
    }

    pub fn from_code(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "720p" | "720p60" | "1280x720" => Some(Self::Hd720p60),
            "900p" | "900p60" | "1600x900" => Some(Self::HdPlus900p60),
            "1080p" | "1080p60" | "1920x1080" => Some(Self::FullHd1080p60),
            _ => None,
        }
    }

    pub const fn cap_for_entitlement(self, can_use_hd_plus: bool) -> Self {
        if can_use_hd_plus {
            self
        } else {
            Self::Hd720p60
        }
    }
}

impl DanserSettings {
    /// Apply one of the supported recording quality profiles (720p60, 900p60, or 1080p60).
    pub fn apply_render_quality(&mut self, quality: RenderQuality) {
        let (w, h, fps) = quality.dimensions();
        self.recording.frame_width = w;
        self.recording.frame_height = h;
        self.recording.fps = fps;
    }

    /// Infer the active supported quality tier from the current recording dimensions.
    pub fn current_render_quality(&self) -> RenderQuality {
        if self.recording.frame_width >= RECORDING_1080P_WIDTH
            && self.recording.frame_height >= RECORDING_1080P_HEIGHT
        {
            RenderQuality::FullHd1080p60
        } else if self.recording.frame_width >= RECORDING_900P_WIDTH
            && self.recording.frame_height >= RECORDING_900P_HEIGHT
        {
            RenderQuality::HdPlus900p60
        } else {
            RenderQuality::Hd720p60
        }
    }

    /// Returns true if the recording dimensions and FPS exactly match `quality`.
    pub fn matches_render_quality(&self, quality: RenderQuality) -> bool {
        let (w, h, fps) = quality.dimensions();
        self.recording.frame_width == w
            && self.recording.frame_height == h
            && self.recording.fps == fps
    }

    /// Returns true if the recording dimensions and FPS match any supported quality tier.
    pub fn has_valid_render_quality(&self) -> bool {
        self.matches_render_quality(RenderQuality::Hd720p60)
            || self.matches_render_quality(RenderQuality::HdPlus900p60)
            || self.matches_render_quality(RenderQuality::FullHd1080p60)
    }

    /// Clamp recording quality to the highest tier permitted by `can_use_hd_plus`.
    pub fn clamp_render_quality(&mut self, can_use_hd_plus: bool) {
        let effective = self.current_render_quality().cap_for_entitlement(can_use_hd_plus);
        self.apply_render_quality(effective);
    }

    /// Ensure `Skin.CurrentSkin` is a bare skin folder name (never a path or dot-segment), since danser
    /// always resolves `CurrentSkin` relative to `General.OsuSkinsDir`.
    pub fn normalize_current_skin(&mut self) {
        let trimmed = self.skin.current_skin.trim();
        let candidate = if trimmed.contains('/') || trimmed.contains('\\') {
            trimmed
                .rsplit(['/', '\\'])
                .find(|seg| !seg.is_empty())
                .unwrap_or("default")
        } else {
            trimmed
        };

        let invalid = candidate.is_empty()
            || matches!(candidate, "." | "..")
            || candidate.len() > 120
            || candidate.chars().any(char::is_control);
        let safe = if invalid { "default" } else { candidate };

        if self.skin.current_skin != safe {
            self.skin.current_skin = safe.to_owned();
        }
    }

    /// Returns true if `Skin.CurrentSkin` is already a normalized bare folder name.
    pub fn has_normalized_skin(&self) -> bool {
        let s = self.skin.current_skin.as_str();
        !s.is_empty()
            && !matches!(s, "." | "..")
            && s.len() <= 120
            && s == s.trim()
            && !s.contains('/')
            && !s.contains('\\')
            && !s.chars().any(char::is_control)
    }

    /// Sanitize user-uploaded JSON settings before saving to disk.
    pub fn sanitize_user_upload(&mut self) {
        self.extra.clear();
        self.general.extra.clear();
        self.graphics.extra.clear();
        self.audio.extra.clear();
        self.input.extra.clear();
        self.gameplay.extra.clear();
        self.skin.extra.clear();
        self.cursor.extra.clear();
        self.objects.extra.clear();
        self.playfield.extra.clear();
        self.cursor_dance.extra.clear();
        self.knockout.extra.clear();
        self.recording.extra.clear();

        self.gameplay.underlay.path.clear();
        if self.gameplay.hud_font.contains(['/', '\\']) || self.gameplay.hud_font.contains("..") {
            self.gameplay.hud_font.clear();
        }
        self.gameplay.results_screen_time = self.gameplay.results_screen_time.clamp(0, 15);

        clamp_finite(&mut self.playfield.lead_in_time, 0.0, 10.0);
        clamp_finite(&mut self.playfield.lead_in_hold, 0.0, 10.0);
        clamp_finite(&mut self.playfield.fade_out_time, 0.0, 10.0);
        clamp_finite(&mut self.playfield.seizure_warning.duration, 0.0, 10.0);

        self.objects.sliders.distortions.viewport_size =
            self.objects.sliders.distortions.viewport_size.min(3840);
        self.objects.sliders.distortions.custom_resolution_x = self
            .objects
            .sliders
            .distortions
            .custom_resolution_x
            .min(3840);
        self.objects.sliders.distortions.custom_resolution_y = self
            .objects
            .sliders
            .distortions
            .custom_resolution_y
            .min(3840);

        self.skin.cursor.long_trail_length = self.skin.cursor.long_trail_length.clamp(0, 10_000);
        self.cursor.trail_max_length = self.cursor.trail_max_length.clamp(0, 10_000);

        clamp_finite(&mut self.skin.cursor.long_trail_density, 0.1, 10.0);
        clamp_finite(&mut self.cursor.trail_density, 0.1, 10.0);
        clamp_finite(&mut self.playfield.background.triangles.density, 0.0, 5.0);
        clamp_finite(&mut self.playfield.background.triangles.scale, 0.1, 5.0);

        self.audio.linux_unix.bass_playback_buffer_length =
            self.audio.linux_unix.bass_playback_buffer_length.clamp(10, 500);
        self.audio.linux_unix.bass_device_buffer_length =
            self.audio.linux_unix.bass_device_buffer_length.clamp(10, 500);
        self.audio.linux_unix.bass_update_period =
            self.audio.linux_unix.bass_update_period.clamp(1, 100);
        self.audio.linux_unix.bass_device_update_period =
            self.audio.linux_unix.bass_device_update_period.clamp(1, 100);

        self.objects.colors.combo_colors.truncate(16);
        self.normalize_current_skin();
        let normalized_quality = self.current_render_quality();
        self.apply_render_quality(normalized_quality);
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct General {
    pub osu_songs_dir: String,
    pub osu_skins_dir: String,
    pub osu_replays_dir: String,
    pub discord_presence_on: bool,
    pub unpack_osz_files: bool,
    pub verbose_import_logs: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Graphics {
    pub width: u32,
    pub height: u32,
    pub window_width: u32,
    pub window_height: u32,
    pub fullscreen: bool,
    #[serde(rename = "VSync")]
    pub vsync: bool,
    #[serde(rename = "FPSCap")]
    pub fpscap: u32,
    #[serde(rename = "MSAA")]
    pub msaa: i8,
    #[serde(rename = "ShowFPS")]
    pub show_fps: bool,
    pub experimental: Experimental,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Experimental {
    pub use_persistent_buffers: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Audio {
    pub general_volume: f64,
    pub music_volume: f64,
    pub sample_volume: f64,
    pub offset: i32,
    pub hitsound_position_multiplier: f64,
    pub ignore_beatmap_samples: bool,
    pub ignore_beatmap_sample_volume: bool,
    pub play_nightcore_samples: bool,
    pub beat_scale: f64,
    pub beat_use_timing_points: bool,
    #[serde(rename = "Linux/Unix")]
    pub linux_unix: LinuxUnix,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LinuxUnix {
    pub bass_playback_buffer_length: i32,
    pub bass_device_buffer_length: i32,
    pub bass_update_period: i64,
    pub bass_device_update_period: i64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Input {
    pub left_key: String,
    pub right_key: String,
    pub restart_key: String,
    pub smoke_key: String,
    pub screenshot_key: String,
    pub mouse_buttons_disabled: bool,
    pub mouse_high_precision: bool,
    pub mouse_sensitivity: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Gameplay {
    pub hit_error_meter: HitErrorMeter,
    pub aim_error_meter: AimErrorMeter,
    pub score: Score,
    pub hp_bar: HpBar,
    pub combo_counter: ComboCounter,
    #[serde(rename = "PPCounter")]
    pub pp_counter: PpCounter,
    pub hit_counter: HitCounter,
    pub strain_graph: StrainGraph,
    pub key_overlay: KeyOverlay,
    pub score_board: ScoreBoard,
    pub mods: Mods,
    pub boundaries: Boundaries,
    pub underlay: Underlay,
    #[serde(rename = "HUDFont")]
    pub hud_font: String,
    pub show_results_screen: bool,
    pub results_screen_time: i32,
    pub results_use_local_time_zone: bool,
    pub show_warning_arrows: bool,
    pub show_hit_lighting: bool,
    pub flashlight_dim: f64,
    pub play_username: String,
    #[serde(rename = "UseLazerPP")]
    pub use_lazer_pp: bool,
    pub always_skip_intro: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HitErrorMeter {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XOffset")]
    pub xoffset: f64,
    #[serde(rename = "YOffset")]
    pub yoffset: f64,
    pub point_fade_out_time: f64,
    pub show_positional_misses: bool,
    pub positional_miss_scale: f64,
    pub show_unstable_rate: bool,
    pub unstable_rate_decimals: u32,
    pub unstable_rate_scale: f64,
    pub static_unstable_rate: bool,
    pub scale_with_speed: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AimErrorMeter {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XPosition")]
    pub xposition: f64,
    #[serde(rename = "YPosition")]
    pub yposition: f64,
    pub point_fade_out_time: f64,
    pub dot_scale: f64,
    pub align: String,
    pub show_unstable_rate: bool,
    pub unstable_rate_scale: f64,
    pub unstable_rate_decimals: u32,
    pub static_unstable_rate: bool,
    pub cap_positional_misses: bool,
    pub angle_normalized: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Score {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XOffset")]
    pub xoffset: f64,
    #[serde(rename = "YOffset")]
    pub yoffset: f64,
    pub progress_bar: String,
    pub show_grade_always: bool,
    pub static_score: bool,
    pub static_accuracy: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HpBar {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XOffset")]
    pub xoffset: f64,
    #[serde(rename = "YOffset")]
    pub yoffset: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ComboCounter {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XOffset")]
    pub xoffset: f64,
    #[serde(rename = "YOffset")]
    pub yoffset: f64,
    #[serde(rename = "Static")]
    pub static_combo: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PpCounter {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XPosition")]
    pub xposition: f64,
    #[serde(rename = "YPosition")]
    pub yposition: f64,
    pub color: Hsv,
    pub decimals: u32,
    pub align: String,
    pub show_in_results: bool,
    #[serde(rename = "ShowPPComponents")]
    pub show_pp_components: bool,
    #[serde(rename = "Static")]
    pub static_pp: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Hsv {
    pub hue: f64,
    pub saturation: f64,
    pub value: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HitCounter {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XPosition")]
    pub xposition: f64,
    #[serde(rename = "YPosition")]
    pub yposition: f64,
    #[serde(rename = "Color300")]
    pub color_300: Hsv,
    #[serde(rename = "Color100")]
    pub color_100: Hsv,
    #[serde(rename = "Color50")]
    pub color_50: Hsv,
    #[serde(rename = "ColorMiss")]
    pub color_miss: Hsv,
    #[serde(rename = "ColorSB")]
    pub color_sb: Hsv,
    pub spacing: i64,
    pub font_scale: i64,
    pub align: String,
    pub value_align: String,
    pub vertical: bool,
    #[serde(rename = "Show300")]
    pub show300: bool,
    #[serde(rename = "ShowSliderBreaks")]
    pub show_sliderbreaks: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StrainGraph {
    pub show: bool,
    pub opacity: f64,
    #[serde(rename = "XPosition")]
    pub xposition: f64,
    #[serde(rename = "YPosition")]
    pub yposition: f64,
    pub align: String,
    pub width: f64,
    pub height: f64,
    pub bg_color: Hsv,
    pub fg_color: Hsv,
    pub outline: StrainGraphOutline,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StrainGraphOutline {
    pub show: bool,
    pub width: f64,
    pub inner_darkness: f64,
    pub inner_opacity: f64,
}

pub type KeyOverlay = HpBar;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ScoreBoard {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XOffset")]
    pub xoffset: f64,
    #[serde(rename = "YOffset")]
    pub yoffset: f64,
    pub mods_only: bool,
    pub align_right: bool,
    pub hide_others: bool,
    pub show_avatars: bool,
    pub explosion_scale: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Mods {
    pub show: bool,
    pub scale: f64,
    pub opacity: f64,
    #[serde(rename = "XOffset")]
    pub xoffset: f64,
    #[serde(rename = "YOffset")]
    pub yoffset: f64,
    pub hide_in_replays: bool,
    pub fold_in_replays: bool,
    pub additional_spacing: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Boundaries {
    pub enabled: bool,
    pub border_thickness: f64,
    pub border_fill: f64,
    pub border_color: Hsv,
    pub border_opacity: f64,
    pub background_color: Hsv,
    pub background_opacity: f64,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Underlay {
    pub path: String,
    pub above_hp_bar: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Skin {
    pub current_skin: String,
    pub fallback_skin: String,
    pub use_colors_from_skin: bool,
    pub use_beatmap_colors: bool,
    pub cursor: SkinCursor,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SkinCursor {
    pub use_skin_cursor: bool,
    pub scale: f64,
    pub trail_scale: f64,
    pub force_long_trail: bool,
    pub long_trail_length: i32,
    pub long_trail_density: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Cursor {
    pub trail_style: u8,
    #[serde(rename = "Style23Speed")]
    pub style23speed: f64,
    #[serde(rename = "Style4Shift")]
    pub style4shift: f64,
    pub colors: CursorColors,
    pub enable_custom_tag_color_offset: bool,
    pub tag_color_offset: f64,
    pub enable_trail_glow: bool,
    pub enable_custom_trail_glow_offset: bool,
    pub trail_glow_offset: i32,
    #[serde(rename = "ScaleToCS")]
    pub scale_to_cs: bool,
    pub cursor_size: i32,
    pub cursor_expand: bool,
    pub scale_to_the_beat: bool,
    pub show_cursors_on_breaks: bool,
    pub bounce_on_edges: bool,
    pub trail_scale: f64,
    pub trail_end_scale: f64,
    pub trail_density: f64,
    pub trail_max_length: i32,
    pub trail_remove_speed: f64,
    pub glow_end_scale: f64,
    pub inner_length_mult: f64,
    pub additive_blending: bool,
    pub cursor_ripples: bool,
    pub smoke_enabled: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct CursorColors {
    pub enable_rainbow: bool,
    pub rainbow_speed: f64,
    pub base_color: Hsv,
    pub enable_custom_hue_offset: bool,
    pub hue_offset: f64,
    pub flash_to_the_beat: bool,
    pub flash_amplitude: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Objects {
    pub draw_approach_circles: bool,
    pub draw_combo_numbers: bool,
    pub draw_follow_points: bool,
    pub load_spinners: bool,
    pub scale_to_the_beat: bool,
    pub stack_enabled: bool,
    pub sliders: Sliders,
    pub colors: Colors,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Sliders {
    pub force_slider_ball_texture: bool,
    pub draw_end_circles: bool,
    pub draw_slider_follow_circle: bool,
    pub draw_score_points: bool,
    pub slider_merge: bool,
    pub border_width: i32,
    pub distortions: SliderDistortions,
    pub snaking: Snaking,
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SliderDistortions {
    enabled: bool,
    viewport_size: u64,
    use_custom_resolution: bool,
    custom_resolution_x: u64,
    custom_resolution_y: u64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Snaking {
    #[serde(rename = "In")]
    pub in_field: bool,
    pub out: bool,
    pub out_fade_instant: bool,
    pub duration_multiplier: f64,
    pub fade_multiplier: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Colors {
    pub mandala_textures_trigger: i32,
    pub mandala_textures_alpha: f64,
    pub color: ObjectColor,
    pub use_combo_colors: bool,
    pub combo_colors: Vec<Hsv>,
    pub use_skin_combo_colors: bool,
    pub use_beatmap_combo_colors: bool,
    pub sliders: ObjectSliders,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ObjectSliders {
    pub white_score_points: bool,
    pub score_point_color_offset: i32,
    pub slider_ball_tint: bool,
    pub border: Border,
    pub body: Body,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Border {
    pub use_hit_circle_color: bool,
    pub color: ObjectColor,
    pub enable_custom_gradient_offset: bool,
    pub custom_gradient_offset: i32,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Body {
    pub use_hit_circle_color: bool,
    pub color: ObjectColor,
    pub inner_offset: f64,
    pub outer_offset: f64,
    pub inner_alpha: f64,
    pub outer_alpha: f64,
}

pub type ObjectColor = CursorColors;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Playfield {
    pub draw_objects: bool,
    pub draw_cursors: bool,
    pub scale: f64,
    pub osu_shift: bool,
    pub shift_y: f64,
    pub shift_x: f64,
    pub scale_storyboard_with_playfield: bool,
    pub move_storyboard_with_playfield: bool,
    pub lead_in_time: f64,
    pub lead_in_hold: f64,
    pub fade_out_time: f64,
    pub seizure_warning: SeizureWarning,
    pub background: Background,
    pub logo: Logo,
    pub bloom: Bloom,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SeizureWarning {
    pub enabled: bool,
    pub duration: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Background {
    pub load_storyboards: bool,
    pub load_videos: bool,
    pub flash_to_the_beat: bool,
    pub dim: Dim,
    pub parallax: Parallax,
    pub blur: Blur,
    pub triangles: Triangles,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Dim {
    pub intro: f64,
    pub normal: f64,
    pub breaks: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Parallax {
    pub enabled: bool,
    pub amount: f64,
    pub speed: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Blur {
    pub enabled: bool,
    pub values: BlurValues,
}

pub type BlurValues = Dim;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Triangles {
    pub enabled: bool,
    pub shadowed: bool,
    pub draw_over_blur: bool,
    pub parallax_multiplier: f64,
    pub density: f64,
    pub scale: f64,
    pub speed: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Logo {
    pub enabled: bool,
    pub draw_spectrum: bool,
    pub dim: Dim,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Bloom {
    pub enabled: bool,
    pub bloom_to_the_beat: bool,
    pub bloom_beat_addition: f64,
    pub threshold: f64,
    pub blur: f64,
    pub power: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct CursorDance {
    pub movers: Vec<Mover>,
    pub spinners: Vec<Spinner>,
    pub combo_tag: bool,
    pub battle: bool,
    pub do_spinners_together: bool,
    #[serde(rename = "TAGSliderDance")]
    pub tag_slider_dance: bool,
    pub mover_settings: MoverSettings,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Mover {
    pub mover: String,
    pub slider_dance: bool,
    pub random_slider_dance: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Spinner {
    pub mover: String,
    pub center_offset_x: f64,
    pub center_offset_y: f64,
    pub radius: i64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MoverSettings {
    pub bezier: Vec<Bezier>,
    pub flower: Vec<Flower>,
    pub half_circle: Vec<HalfCircle>,
    pub spline: Vec<Spline>,
    pub momentum: Vec<Momentum>,
    pub ex_gon: Vec<ExGon>,
    pub linear: Vec<Linear>,
    pub pippi: Vec<Pippi>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Bezier {
    pub aggressiveness: f64,
    pub slider_aggressiveness: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Flower {
    pub angle_offset: f64,
    pub distance_mult: f64,
    pub stream_angle_offset: f64,
    pub long_jump: i32,
    pub long_jump_mult: f64,
    pub long_jump_on_equal_pos: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HalfCircle {
    pub radius_multiplier: f64,
    pub stream_trigger: i64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Spline {
    pub rotational_force: bool,
    pub stream_half_circle: bool,
    pub stream_wobble: bool,
    pub wobble_scale: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Momentum {
    pub skip_stack_angles: bool,
    pub stream_restrict: bool,
    pub duration_mult: f64,
    pub duration_trigger: f64,
    pub stream_mult: f64,
    pub restrict_angle: f64,
    pub restrict_area: f64,
    pub restrict_invert: bool,
    pub distance_mult: f64,
    pub distance_mult_out: f64,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ExGon {
    pub delay: u32,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Linear {
    pub wait_for_preempt: bool,
    pub reaction_time: f64,
    pub choppy_long_objects: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Pippi {
    pub rotation_speed: f64,
    pub radius_multiplier: f64,
    pub spinner_radius: i64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Knockout {
    pub mode: u8,
    pub grace_end_time: i32,
    pub bubble_minimum_combo: u32,
    pub exclude_mods: String,
    pub hide_mods: String,
    pub max_players: u32,
    pub min_players: u32,
    pub revive_players_at_end: bool,
    pub live_sort: bool,
    pub sort_by: String,
    pub hide_overlay_on_breaks: bool,
    pub min_cursor_size: f64,
    pub max_cursor_size: f64,
    pub add_danser: bool,
    pub danser_name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Recording {
    pub frame_width: i32,
    pub frame_height: i32,
    #[serde(rename = "FPS")]
    pub fps: i32,
    #[serde(rename = "EncodingFPSCap")]
    pub encoding_fpscap: i32,
    pub encoder: String,
    #[serde(rename = "libx264")]
    pub libx264: Libx264,
    #[serde(rename = "libx265")]
    pub libx265: Libx265,
    #[serde(rename = "h264_nvenc")]
    pub h264_nvenc: H264Nvenc,
    #[serde(rename = "hevc_nvenc")]
    pub hevc_nvenc: HevcNvenc,
    #[serde(rename = "h264_qsv")]
    pub h264_qsv: H264Qsv,
    #[serde(rename = "hevc_qsv")]
    pub hevc_qsv: HevcQsv,
    #[serde(rename = "custom")]
    pub custom: Custom,
    pub pixel_format: String,
    pub filters: String,
    pub audio_codec: String,
    #[serde(rename = "aac")]
    pub aac: Aac,
    #[serde(rename = "libmp3lame")]
    pub libmp3lame: Libmp3lame,
    #[serde(rename = "libopus")]
    pub libopus: Libopus,
    #[serde(rename = "flac")]
    pub flac: Flac,
    #[serde(rename = "customAudio")]
    pub custom_audio: CustomAudio,
    pub audio_filters: String,
    pub output_dir: String,
    pub container: String,
    #[serde(rename = "ShowFFmpegLogs")]
    pub show_ffmpeg_logs: bool,
    pub motion_blur: MotionBlur,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Libx264 {
    pub rate_control: String,
    pub bitrate: String,
    #[serde(rename = "CRF")]
    pub crf: i64,
    pub profile: String,
    pub preset: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Libx265 {
    pub rate_control: String,
    pub bitrate: String,
    #[serde(rename = "CRF")]
    pub crf: i64,
    pub preset: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct H264Nvenc {
    pub rate_control: String,
    pub bitrate: String,
    #[serde(rename = "CQ")]
    pub cq: i64,
    pub profile: String,
    pub preset: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HevcNvenc {
    pub rate_control: String,
    pub bitrate: String,
    #[serde(rename = "CQ")]
    pub cq: i64,
    pub preset: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct H264Qsv {
    pub rate_control: String,
    pub bitrate: String,
    pub quality: i64,
    pub profile: String,
    pub preset: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HevcQsv {
    pub rate_control: String,
    pub bitrate: String,
    pub quality: i64,
    pub preset: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Custom {
    pub custom_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Aac {
    pub bitrate: String,
    pub additional_options: String,
}

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Libmp3lame {
    pub rate_control: String,
    pub target_bitrate: String,
    pub additional_options: String,
}

pub type Libopus = Libmp3lame;

#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Flac {
    pub compression_level: i64,
    pub additional_options: String,
}

pub type CustomAudio = Custom;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MotionBlur {
    pub enabled: bool,
    pub oversample_multiplier: u32,
    pub blend_frames: u32,
    #[serde(rename = "BlendFunctionID")]
    pub blend_function_id: u32,
    pub gauss_weights_mult: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danser_settings_preserves_unknown_fields_on_roundtrip() {
        let sample = serde_json::json!({
            "General": { "OsuSongsDir": "./Songs" },
            "Audio": { "MusicVolume": 0.5, "OnlineOffset": 12 },
            "Gameplay": {
                "PlayUsername": "Guest",
                "PPVersion": "2025",
                "LazerClassicScore": true
            },
            "Recording": {
                "Encoder": "libsvtav1",
                "libsvtav1": { "Preset": 8 }
            },
            "Debug": { "PerfGraph": { "Show": false } }
        });

        let parsed: DanserSettings = serde_json::from_value(sample).unwrap();
        let out = serde_json::to_value(&parsed).unwrap();

        assert_eq!(out["Audio"]["OnlineOffset"], 12);
        assert_eq!(out["Gameplay"]["PPVersion"], "2025");
        assert_eq!(out["Gameplay"]["LazerClassicScore"], true);
        assert_eq!(out["Recording"]["libsvtav1"]["Preset"], 8);
        assert_eq!(out["Debug"]["PerfGraph"]["Show"], false);
    }

    #[test]
    fn root_settings_ignores_legacy_output_channel_on_load() {
        let sample = serde_json::json!({
            "Servers": [{
                "server_id": "111111111111111111",
                "input_channels": ["222222222222222222", "333333333333333333"],
                "output_channel": "222222222222222222",
                "prefix": "<",
                "ai_channel": null
            }]
        });

        let parsed: RootSettings = serde_json::from_value(sample).unwrap();
        let guild_id = Id::new(111111111111111111);
        let guard = parsed.servers.pin();
        let server = guard.get(&guild_id).expect("guild should be loaded");
        assert_eq!(server.input_channels.len(), 2);
        assert_eq!(server.effective_prefix(), "<");
        drop(guard);

        let serialized = serde_json::to_value(&parsed).unwrap();
        let server_obj = serialized["Servers"][0].as_object().unwrap();
        assert!(!server_obj.contains_key("output_channel"));
    }

    #[test]
    fn danser_settings_sanitize_user_upload_clamps_and_truncates() {
        let mut settings = DanserSettings::default();
        settings.skin.cursor.long_trail_density = f64::INFINITY;
        settings.cursor.trail_density = 9_999.0;
        settings.playfield.background.triangles.density = 999.0;
        settings.playfield.background.triangles.scale = -5.0;
        settings.audio.linux_unix.bass_playback_buffer_length = 9_999_999;
        settings.audio.linux_unix.bass_device_buffer_length = 1;
        settings.audio.linux_unix.bass_update_period = 0;
        settings.audio.linux_unix.bass_device_update_period = 50_000;
        settings.objects.colors.combo_colors = vec![Hsv::default(); 50];
        settings.skin.current_skin = "/opt/danser/Skins/roundtable2026_lite".to_owned();

        settings.sanitize_user_upload();

        assert!((settings.skin.cursor.long_trail_density - 0.1).abs() < f64::EPSILON);
        assert!((settings.cursor.trail_density - 10.0).abs() < f64::EPSILON);
        assert!((settings.playfield.background.triangles.density - 5.0).abs() < f64::EPSILON);
        assert!((settings.playfield.background.triangles.scale - 0.1).abs() < f64::EPSILON);
        assert_eq!(settings.audio.linux_unix.bass_playback_buffer_length, 500);
        assert_eq!(settings.audio.linux_unix.bass_device_buffer_length, 10);
        assert_eq!(settings.audio.linux_unix.bass_update_period, 1);
        assert_eq!(settings.audio.linux_unix.bass_device_update_period, 100);
        assert_eq!(settings.objects.colors.combo_colors.len(), 16);
        assert!(settings.matches_render_quality(RenderQuality::Hd720p60));
        assert_eq!(settings.recording.frame_width, RECORDING_720P_WIDTH);
        assert_eq!(settings.recording.frame_height, RECORDING_720P_HEIGHT);
        assert_eq!(settings.recording.fps, LOCKED_RECORDING_FPS);
        settings.apply_render_quality(RenderQuality::HdPlus900p60);
        assert!(settings.matches_render_quality(RenderQuality::HdPlus900p60));
        assert_eq!(settings.recording.frame_width, RECORDING_900P_WIDTH);
        assert_eq!(settings.recording.frame_height, RECORDING_900P_HEIGHT);
        settings.apply_render_quality(RenderQuality::FullHd1080p60);
        assert!(settings.matches_render_quality(RenderQuality::FullHd1080p60));
        assert_eq!(settings.recording.frame_width, RECORDING_1080P_WIDTH);
        assert_eq!(settings.recording.frame_height, RECORDING_1080P_HEIGHT);
        assert_eq!(RenderQuality::from_code("1080p"), Some(RenderQuality::FullHd1080p60));
        settings.clamp_render_quality(false);
        assert!(settings.matches_render_quality(RenderQuality::Hd720p60));
        assert_eq!(settings.skin.current_skin, "roundtable2026_lite");
        assert!(settings.has_normalized_skin());
    }
}
