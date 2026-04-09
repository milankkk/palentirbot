use rosu_v2::prelude::{Beatmap, GameMode, RankStatus};

pub trait BeatmapExt: Send + Sync {
    fn max_combo(&self) -> Option<u32>;
    fn map_id(&self) -> u32;
    fn mode(&self) -> GameMode;
    fn stars(&self) -> Option<f32>;
    fn rank_status(&self) -> RankStatus;
}

impl BeatmapExt for Beatmap {
    fn max_combo(&self) -> Option<u32> {
        self.max_combo
    }
    fn map_id(&self) -> u32 {
        self.map_id
    }
    fn mode(&self) -> GameMode {
        self.mode
    }
    fn stars(&self) -> Option<f32> {
        Some(self.stars)
    }
    fn rank_status(&self) -> RankStatus {
        self.status
    }
}
