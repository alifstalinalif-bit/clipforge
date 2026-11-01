//! ClipForge core: mesin timeline (track, clip, split/trim/move, undo/redo).
//!
//! Semua waktu dalam mikrodetik (`Micros`) biar presisi dan nggak ada
//! masalah pembulatan float.

use serde::{Deserialize, Serialize};

pub type Micros = i64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: u64,
    /// Path / URI sumber media (content:// di Android).
    pub source: String,
    /// Titik mulai di dalam file sumber.
    pub source_in: Micros,
    /// Panjang clip di timeline.
    pub duration: Micros,
    /// Posisi mulai clip di timeline.
    pub start: Micros,
}

impl Clip {
    pub fn end(&self) -> Micros {
        self.start + self.duration
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: u64,
    pub kind: TrackKind,
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    NoSuchTrack,
    NoSuchClip,
    /// Parameter di luar jangkauan (durasi <= 0, titik split di luar clip, dll).
    OutOfRange,
    /// Clip bakal numpuk sama clip lain di track yang sama.
    Overlap,
}

#[derive(Debug, Default)]
pub struct Timeline {
    tracks: Vec<Track>,
    next_id: u64,
    undo: Vec<Vec<Track>>,
    redo: Vec<Vec<Track>>,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Default::default()
        }
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Total durasi timeline (ujung clip paling akhir).
    pub fn duration(&self) -> Micros {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(Clip::end)
            .max()
            .unwrap_or(0)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.tracks).unwrap_or_else(|_| "[]".into())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Jalankan satu edit secara atomik: kalau gagal, state balik seperti semula
    /// dan nggak masuk riwayat undo.
    ///
    /// Catatan: pakai snapshot penuh biar simpel. Kalau proyek sudah gede,
    /// ganti ke pola command (simpan diff per operasi).
    fn edit<T>(
        &mut self,
        f: impl FnOnce(&mut Vec<Track>, &mut u64) -> Result<T, EditError>,
    ) -> Result<T, EditError> {
        let before = self.tracks.clone();
        match f(&mut self.tracks, &mut self.next_id) {
            Ok(v) => {
                self.undo.push(before);
                self.redo.clear();
                Ok(v)
            }
            Err(e) => {
                self.tracks = before;
                Err(e)
            }
        }
    }

    pub fn add_track(&mut self, kind: TrackKind) -> u64 {
        self.edit(|tracks, next_id| {
            let id = *next_id;
            *next_id += 1;
            tracks.push(Track {
                id,
                kind,
                clips: Vec::new(),
            });
            Ok(id)
        })
        .expect("add_track nggak mungkin gagal")
    }

    pub fn add_clip(
        &mut self,
        track_id: u64,
        source: &str,
        source_in: Micros,
        duration: Micros,
        start: Micros,
    ) -> Result<u64, EditError> {
        if duration <= 0 || source_in < 0 || start < 0 {
            return Err(EditError::OutOfRange);
        }
        self.edit(|tracks, next_id| {
            let track = tracks
                .iter_mut()
                .find(|t| t.id == track_id)
                .ok_or(EditError::NoSuchTrack)?;
            let id = *next_id;
            let clip = Clip {
                id,
                source: source.to_string(),
                source_in,
                duration,
                start,
            };
            if overlaps(&track.clips, None, clip.start, clip.end()) {
                return Err(EditError::Overlap);
            }
            *next_id += 1;
            track.clips.push(clip);
            track.clips.sort_by_key(|c| c.start);
            Ok(id)
        })
    }

    /// Potong clip jadi dua di posisi `at` (waktu timeline).
    /// Clip kiri tetap pakai id lama, clip kanan dapat id baru (dikembalikan).
    pub fn split(&mut self, clip_id: u64, at: Micros) -> Result<u64, EditError> {
        self.edit(|tracks, next_id| {
            let (ti, ci) = locate(tracks, clip_id)?;
            let clip = &mut tracks[ti].clips[ci];
            if at <= clip.start || at >= clip.end() {
                return Err(EditError::OutOfRange);
            }
            let left_len = at - clip.start;
            let right = Clip {
                id: *next_id,
                source: clip.source.clone(),
                source_in: clip.source_in + left_len,
                duration: clip.duration - left_len,
                start: at,
            };
            *next_id += 1;
            clip.duration = left_len;
            let rid = right.id;
            tracks[ti].clips.insert(ci + 1, right);
            Ok(rid)
        })
    }

    /// Geser awal clip ke `new_start` (waktu timeline). Isi di dalam clip ikut
    /// terpotong; bisa juga diperpanjang ke kiri selama sumbernya masih ada.
    pub fn trim_start(&mut self, clip_id: u64, new_start: Micros) -> Result<(), EditError> {
        self.edit(|tracks, _| {
            let (ti, ci) = locate(tracks, clip_id)?;
            let (start, end, source_in) = {
                let c = &tracks[ti].clips[ci];
                (c.start, c.end(), c.source_in)
            };
            let delta = new_start - start;
            if new_start < 0 || new_start >= end || source_in + delta < 0 {
                return Err(EditError::OutOfRange);
            }
            if overlaps(&tracks[ti].clips, Some(clip_id), new_start, end) {
                return Err(EditError::Overlap);
            }
            let c = &mut tracks[ti].clips[ci];
            c.start = new_start;
            c.source_in += delta;
            c.duration = end - new_start;
            Ok(())
        })
    }

    /// Geser akhir clip ke `new_end` (waktu timeline).
    pub fn trim_end(&mut self, clip_id: u64, new_end: Micros) -> Result<(), EditError> {
        self.edit(|tracks, _| {
            let (ti, ci) = locate(tracks, clip_id)?;
            let start = tracks[ti].clips[ci].start;
            if new_end <= start {
                return Err(EditError::OutOfRange);
            }
            if overlaps(&tracks[ti].clips, Some(clip_id), start, new_end) {
                return Err(EditError::Overlap);
            }
            tracks[ti].clips[ci].duration = new_end - start;
            Ok(())
        })
    }

    pub fn move_clip(&mut self, clip_id: u64, new_start: Micros) -> Result<(), EditError> {
        self.edit(|tracks, _| {
            let (ti, ci) = locate(tracks, clip_id)?;
            if new_start < 0 {
                return Err(EditError::OutOfRange);
            }
            let dur = tracks[ti].clips[ci].duration;
            if overlaps(&tracks[ti].clips, Some(clip_id), new_start, new_start + dur) {
                return Err(EditError::Overlap);
            }
            tracks[ti].clips[ci].start = new_start;
            tracks[ti].clips.sort_by_key(|c| c.start);
            Ok(())
        })
    }

    pub fn delete_clip(&mut self, clip_id: u64) -> Result<(), EditError> {
        self.edit(|tracks, _| {
            let (ti, ci) = locate(tracks, clip_id)?;
            tracks[ti].clips.remove(ci);
            Ok(())
        })
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                let current = std::mem::replace(&mut self.tracks, prev);
                self.redo.push(current);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(next) => {
                let current = std::mem::replace(&mut self.tracks, next);
                self.undo.push(current);
                true
            }
            None => false,
        }
    }
}

fn locate(tracks: &[Track], clip_id: u64) -> Result<(usize, usize), EditError> {
    for (ti, t) in tracks.iter().enumerate() {
        if let Some(ci) = t.clips.iter().position(|c| c.id == clip_id) {
            return Ok((ti, ci));
        }
    }
    Err(EditError::NoSuchClip)
}

fn overlaps(clips: &[Clip], ignore: Option<u64>, start: Micros, end: Micros) -> bool {
    clips
        .iter()
        .filter(|c| Some(c.id) != ignore)
        .any(|c| c.start < end && start < c.end())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Micros = 1_000_000;

    fn setup() -> (Timeline, u64, u64) {
        let mut tl = Timeline::new();
        let track = tl.add_track(TrackKind::Video);
        let clip = tl.add_clip(track, "a.mp4", 0, 10 * SEC, 0).unwrap();
        (tl, track, clip)
    }

    #[test]
    fn split_membagi_clip_dan_geser_source_in() {
        let (mut tl, _, clip) = setup();
        let right = tl.split(clip, 4 * SEC).unwrap();
        let clips = &tl.tracks()[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].duration, 4 * SEC);
        assert_eq!(clips[1].id, right);
        assert_eq!(clips[1].start, 4 * SEC);
        assert_eq!(clips[1].source_in, 4 * SEC);
        assert_eq!(clips[1].duration, 6 * SEC);
    }

    #[test]
    fn split_di_tepi_ditolak() {
        let (mut tl, _, clip) = setup();
        assert_eq!(tl.split(clip, 0), Err(EditError::OutOfRange));
        assert_eq!(tl.split(clip, 10 * SEC), Err(EditError::OutOfRange));
    }

    #[test]
    fn trim_start_dan_end() {
        let (mut tl, _, clip) = setup();
        tl.trim_start(clip, 2 * SEC).unwrap();
        tl.trim_end(clip, 8 * SEC).unwrap();
        let c = &tl.tracks()[0].clips[0];
        assert_eq!((c.start, c.source_in, c.duration), (2 * SEC, 2 * SEC, 6 * SEC));
    }

    #[test]
    fn trim_start_ke_kiri_nggak_boleh_lewat_awal_sumber() {
        let (mut tl, _, clip) = setup();
        tl.move_clip(clip, 5 * SEC).unwrap();
        // source_in = 0, jadi nggak bisa diperpanjang ke kiri
        assert_eq!(tl.trim_start(clip, 3 * SEC), Err(EditError::OutOfRange));
    }

    #[test]
    fn overlap_ditolak_dan_state_tetap() {
        let (mut tl, track, _) = setup();
        let before = tl.to_json();
        assert_eq!(
            tl.add_clip(track, "b.mp4", 0, 5 * SEC, 5 * SEC),
            Err(EditError::Overlap)
        );
        assert_eq!(tl.to_json(), before);
        assert!(tl.add_clip(track, "b.mp4", 0, 5 * SEC, 10 * SEC).is_ok());
        assert_eq!(tl.duration(), 15 * SEC);
    }

    #[test]
    fn undo_redo_bolak_balik() {
        let (mut tl, _, clip) = setup();
        tl.split(clip, 4 * SEC).unwrap();
        assert_eq!(tl.tracks()[0].clips.len(), 2);
        assert!(tl.undo());
        assert_eq!(tl.tracks()[0].clips.len(), 1);
        assert!(tl.redo());
        assert_eq!(tl.tracks()[0].clips.len(), 2);
    }

    #[test]
    fn edit_baru_menghapus_redo() {
        let (mut tl, _, clip) = setup();
        tl.split(clip, 4 * SEC).unwrap();
        tl.undo();
        tl.trim_end(clip, 5 * SEC).unwrap();
        assert!(!tl.can_redo());
    }

    #[test]
    fn edit_gagal_nggak_masuk_undo() {
        let (mut tl, _, clip) = setup();
        let n = tl.undo.len();
        let _ = tl.split(clip, 0);
        assert_eq!(tl.undo.len(), n);
    }

    #[test]
    fn delete_clip_dan_undo() {
        let (mut tl, _, clip) = setup();
        tl.delete_clip(clip).unwrap();
        assert_eq!(tl.duration(), 0);
        tl.undo();
        assert_eq!(tl.duration(), 10 * SEC);
    }
}
