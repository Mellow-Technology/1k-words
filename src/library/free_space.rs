//! Freeing up space by removing originals that are safely synced, to keep a
//! library within its budget on this device. They're fetched again as
//! they're wanted (see `download`).
//!
//! The budget is a cap on the space originals take, as a share of the
//! library's volume, and a floor on the volume's free space. Going over the
//! cap or under the floor is the high water mark: originals are then removed
//! until both are clear by `LOW_WATER_MARGIN`, so that one more import
//! doesn't set it off again. Pins count toward the cap, though they can't be
//! removed to meet it.
//!
//! What goes, in order:
//!
//! 1. A photo's raw file when it has a JPEG to show, and the frames of a
//!    stack other than its cover, from the groups wanted longest ago on.
//! 2. What's left of whole groups, a group being a stack (or a photo on its
//!    own), from those wanted longest ago on.
//!
//! A group was last wanted at the latest of its frames' capture dates and
//! when they were last opened here. A group only goes whole: if any of its
//! files have to stay, its cover stays too.
//!
//! An original is only removed if it's not pinned, if its thumbnail's here
//! for showing it, and if enough targets hold a confirmed copy of it. A copy
//! is confirmed if the target's sync record says it's there and it's the
//! size of the file here. A copy only counts if its target can be reached
//! now and can download it back. Nothing's removed until the catalog itself
//! is synced, as it's the only record of where the copies are.
//!
//! Like the rest of the catalog, this blocks; call it from a background
//! thread.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::Library;
use super::catalog::{SyncItemRecord, SyncState};
use super::download::{DownloadSource, download_plugin};
use super::on_device::OriginalsUsage;
use crate::photos::{self, Stack};
use crate::plugins::Plugins;

const GB: u64 = 1_000_000_000;
/// How far past the marks removal goes once it's started, as a fraction of
/// the cap (`1 / LOW_WATER_MARGIN`).
const LOW_WATER_MARGIN: u64 = 10;

/// How much originals may take on this device, and what it takes for them to
/// be removed. Kept in the app's settings, which aren't synced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpaceBudget {
    /// The cap on originals, as a percentage of the volume.
    pub cap_percent: u32,
    /// The free space to keep on the volume, in gigabytes.
    pub floor_gb: u32,
    /// How many targets must hold a copy of an original to remove it.
    pub copies: u32,
}

impl Default for SpaceBudget {
    fn default() -> Self {
        Self {
            cap_percent: 25,
            floor_gb: 10,
            copies: 1,
        }
    }
}

impl SpaceBudget {
    /// The cap in bytes, on a volume of `total` bytes. Settings are only
    /// ever a file, so a nonsense cap can't ask for everything to go.
    pub fn cap(&self, total: u64) -> u64 {
        let percent = u128::from(self.cap_percent.clamp(1, 100));
        (u128::from(total) * percent / 100) as u64
    }

    /// There's always at least one copy, whatever the settings say.
    fn copies(&self) -> usize {
        self.copies.max(1) as usize
    }

    /// How much to remove, with originals taking `used` bytes of `volume`.
    fn excess(&self, used: u64, volume: Volume) -> u64 {
        let cap = self.cap(volume.total);
        let floor = u64::from(self.floor_gb) * GB;
        if used <= cap && volume.free >= floor {
            return 0;
        }
        let margin = cap / LOW_WATER_MARGIN;
        let for_cap = used.saturating_sub(cap - margin);
        let for_floor = (floor + margin).saturating_sub(volume.free);
        for_cap.max(for_floor)
    }
}

/// The volume a library's on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub total: u64,
    pub free: u64,
}

impl Volume {
    fn of(path: &Path) -> Result<Self> {
        Ok(Self {
            total: fs4::total_space(path).context("reading the volume's size")?,
            free: fs4::available_space(path).context("reading the volume's free space")?,
        })
    }
}

/// How a library's originals stand on this device.
#[derive(Clone, Debug, PartialEq)]
pub struct SpaceOverview {
    pub usage: OriginalsUsage,
    /// Photos pinned themselves, however many more their albums pin.
    pub pinned_photos: usize,
    pub pinned_albums: Vec<String>,
    /// If it could be read.
    pub volume: Option<Volume>,
}

#[derive(Debug, Default, PartialEq)]
pub struct FreeUpSummary {
    /// Originals removed, and their size.
    pub removed: usize,
    pub bytes: u64,
    /// How much more there was to remove but had to stay.
    pub short: u64,
    /// Why it stopped before the end, if it did.
    pub stopped: Option<String>,
}

impl FreeUpSummary {
    fn stopped(why: impl Into<String>) -> Self {
        Self {
            stopped: Some(why.into()),
            ..Self::default()
        }
    }
}

/// An original that's here.
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    path: String,
    size: u64,
    /// Whether it can go (see the module's description).
    removable: bool,
}

/// A stack or a lone photo, as far as what's here goes.
#[derive(Clone, Debug, Default, PartialEq)]
struct Group {
    /// When it was last wanted.
    last_used: f64,
    /// What can go ahead of the rest: raw pairs and other frames.
    lighten: Vec<Candidate>,
    /// The cover's displayed file and Live Photo video.
    rest: Vec<Candidate>,
}

/// What to remove to free `need` bytes, in order, each entry being removed
/// as one. Comes up short if too much has to stay.
fn plan(mut groups: Vec<Group>, need: u64) -> Vec<Vec<Candidate>> {
    groups.sort_by(|a, b| a.last_used.total_cmp(&b.last_used));
    let mut units = Vec::new();
    let mut planned = 0;
    for file in groups.iter().flat_map(|group| &group.lighten) {
        if planned >= need {
            return units;
        }
        if file.removable {
            planned += file.size;
            units.push(vec![file.clone()]);
        }
    }
    for group in &groups {
        if planned >= need {
            break;
        }
        let whole = group.lighten.iter().chain(&group.rest);
        if group.rest.is_empty() || !whole.into_iter().all(|file| file.removable) {
            continue;
        }
        planned += group.rest.iter().map(|file| file.size).sum::<u64>();
        units.push(group.rest.clone());
    }
    units
}

/// Whether a target's record of a file of `size` bytes confirms it's there.
fn confirmed(item: &SyncItemRecord, size: u64) -> bool {
    item.state == SyncState::Synced && item.remote_key.is_some() && item.size == Some(size)
}

/// What's known of a library's originals, to choose among them.
struct Originals {
    /// Those here, and their sizes.
    here: HashMap<String, u64>,
    opened: HashMap<String, f64>,
    pinned: HashSet<String>,
    /// How many reachable targets hold each.
    copies: HashMap<String, usize>,
    required: usize,
}

impl Originals {
    /// `file`, if it's here.
    fn candidate(&self, file: &str, shown: bool) -> Option<Candidate> {
        let copies = self.copies.get(file).copied().unwrap_or(0);
        Some(Candidate {
            path: file.to_owned(),
            size: *self.here.get(file)?,
            removable: shown && copies >= self.required && !self.pinned.contains(file),
        })
    }
}

impl Library {
    /// Whether the catalog itself is safely on a target. It's the only
    /// record of which originals are where, so until it is, removing them
    /// risks losing track of them.
    ///
    /// Nothing syncs the catalog yet (KW-26), so this can't be true, and no
    /// originals are removed. It's for KW-26 to answer properly.
    fn catalog_synced(&self) -> bool {
        false
    }

    /// Removes originals to bring the library within `budget`, if it's
    /// outside it, going by `sources`: the targets set up on this device.
    /// Does nothing if it's already doing this.
    pub fn free_up_space(
        &self,
        plugins: &Plugins,
        sources: &[DownloadSource],
        budget: &SpaceBudget,
    ) -> Result<FreeUpSummary> {
        if !self.catalog_synced() {
            return Ok(FreeUpSummary::stopped(
                "Originals stay on this device until the library's catalog is synced.",
            ));
        }
        let mut summary = FreeUpSummary::default();
        self.freeing.run(|| {
            self.refresh_originals()?;
            let volume = Volume::of(&self.info.path)?;
            summary = self.remove_to_budget(plugins, sources, budget, volume)?;
            Ok(summary.removed)
        })?;
        Ok(summary)
    }

    /// `free_up_space`, once it's clear originals can go at all, with the
    /// library's records up to date.
    fn remove_to_budget(
        &self,
        plugins: &Plugins,
        sources: &[DownloadSource],
        budget: &SpaceBudget,
        volume: Volume,
    ) -> Result<FreeUpSummary> {
        self.check_folder()?;
        let need = budget.excess(self.originals_usage()?.bytes, volume);
        if need == 0 {
            return Ok(FreeUpSummary::default());
        }
        let reachable = self.reachable(plugins, sources)?;
        if reachable.len() < budget.copies() {
            return Ok(FreeUpSummary {
                short: need,
                ..FreeUpSummary::stopped(format!(
                    "Originals stay on this device: {} of the {} targets needed can be reached.",
                    reachable.len(),
                    budget.copies()
                ))
            });
        }
        let originals = self.originals(&reachable, budget)?;
        let stacks = photos::stacks(self.photos()?);
        let groups = stacks
            .iter()
            .map(|stack| self.group(stack, &originals))
            .collect::<Result<Vec<_>>>()?;

        // A photo pinned from here on is downloaded again (`download_pinned`).
        let mut summary = FreeUpSummary::default();
        'units: for unit in plan(groups, need) {
            // It'd look like everything's gone, or the files aren't there
            // to remove, if the disk's been unplugged.
            if let Err(error) = self.check_folder() {
                summary.stopped = Some(format!("{error:#}"));
                break;
            }
            for file in unit {
                if let Err(error) = self.remove_original_file(&file.path) {
                    summary.stopped = Some(format!("{error:#}"));
                    break 'units;
                }
                summary.removed += 1;
                summary.bytes += file.size;
            }
        }
        summary.short = need.saturating_sub(summary.bytes);
        Ok(summary)
    }

    /// Those of `sources` that can be reached now, and can give back what
    /// they hold.
    fn reachable<'a>(
        &self,
        plugins: &Plugins,
        sources: &'a [DownloadSource],
    ) -> Result<Vec<&'a DownloadSource>> {
        let mut reachable = Vec::new();
        for source in sources {
            if self.sync_target(source.id)?.error.is_some() {
                continue;
            }
            let Ok(plugin) = download_plugin(plugins, &source.settings) else {
                continue;
            };
            let answer = plugin
                .session(source.settings.config.clone())
                .and_then(|mut session| session.validate());
            if matches!(answer, Ok(Ok(()))) {
                reachable.push(source);
            }
        }
        Ok(reachable)
    }

    fn originals(&self, reachable: &[&DownloadSource], budget: &SpaceBudget) -> Result<Originals> {
        let here: HashMap<String, u64> = self
            .on_device
            .originals()?
            .into_iter()
            .map(|(path, (size, _))| (path, size))
            .collect();
        let mut copies: HashMap<String, usize> = HashMap::new();
        for source in reachable {
            for (path, item) in self.sync_items(source.id)? {
                if here.get(&path).is_some_and(|size| confirmed(&item, *size)) {
                    *copies.entry(path).or_default() += 1;
                }
            }
        }
        Ok(Originals {
            here,
            opened: self.last_opened()?,
            pinned: self.pinned_originals()?,
            copies,
            required: budget.copies(),
        })
    }

    /// What of `stack` is here.
    fn group(&self, stack: &Stack, originals: &Originals) -> Result<Group> {
        let mut group = Group::default();
        let mut frames = Vec::new();
        for (ix, photo) in stack.photos.iter().enumerate() {
            let displayed = self.relative(&photo.path)?;
            let taken = photo.meta.captured_at.unwrap_or(photo.meta.modified_at);
            let opened = originals.opened.get(&displayed).copied().unwrap_or(0.0);
            group.last_used = group.last_used.max(taken).max(opened);

            if let Some(raw) = &photo.raw_pair {
                let raw = self.relative(raw)?;
                group.lighten.extend(originals.candidate(&raw, true));
            }
            // With no thumbnail either, there'd be nothing to show for it.
            let shown = photo.thumbnail.as_ref().is_some_and(|file| file.is_file());
            let video = photo.live_video.as_ref();
            let video = video.map(|file| self.relative(file)).transpose()?;
            let files = std::iter::once(&displayed).chain(&video);
            let files = files.filter_map(|file| originals.candidate(file, shown));
            if ix == stack.cover {
                group.rest.extend(files);
            } else {
                frames.extend(files);
            }
        }
        group.lighten.extend(frames);
        Ok(group)
    }

    /// How the library's originals stand, as of the last records.
    pub fn space_overview(&self) -> Result<SpaceOverview> {
        let pinned = self.pinned_albums()?;
        let pinned_albums = self
            .albums()?
            .into_iter()
            .filter(|album| pinned.contains(&album.id))
            .map(|album| album.name)
            .collect();
        Ok(SpaceOverview {
            usage: self.originals_usage()?,
            pinned_photos: self.on_device.pinned_photos()?.len(),
            pinned_albums,
            volume: Volume::of(&self.info.path).ok(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::download::tests::{Synced, synced};
    use super::super::{Id, LibraryHandle, protect, test_support};
    use super::*;
    use crate::photos::Photo;

    const VOLUME: Volume = Volume {
        total: 1000 * GB,
        free: 500 * GB,
    };

    #[test]
    fn stays_put_until_past_a_high_water_mark() {
        let budget = SpaceBudget::default();
        assert_eq!(budget.cap(VOLUME.total), 250 * GB);
        assert_eq!(budget.excess(250 * GB, VOLUME), 0);
        let free = |free| Volume { free, ..VOLUME };
        assert_eq!(budget.excess(100 * GB, free(10 * GB)), 0);
    }

    #[test]
    fn goes_down_to_the_low_water_marks_once_past_one() {
        let budget = SpaceBudget::default();
        // 25 GB under the cap, from 1 GB over it.
        assert_eq!(budget.excess(251 * GB, VOLUME), 26 * GB);
        // 25 GB over the floor, from 1 GB under it.
        let low = Volume {
            free: 9 * GB,
            ..VOLUME
        };
        assert_eq!(budget.excess(100 * GB, low), 26 * GB);
        // Whichever's further.
        assert_eq!(budget.excess(300 * GB, low), 75 * GB);
    }

    #[test]
    fn nonsense_settings_ask_for_little_and_keep_a_copy() {
        let budget = SpaceBudget {
            cap_percent: 0,
            floor_gb: 0,
            copies: 0,
        };
        assert_eq!(budget.cap(VOLUME.total), 10 * GB);
        assert_eq!(budget.copies(), 1);
        let huge = SpaceBudget {
            cap_percent: 1000,
            ..budget
        };
        assert_eq!(huge.cap(VOLUME.total), VOLUME.total);
    }

    #[test]
    fn budget_settings_fill_in_defaults() {
        let budget: SpaceBudget = serde_json::from_str(r#"{"copies":2}"#).unwrap();
        assert_eq!(
            budget,
            SpaceBudget {
                copies: 2,
                ..SpaceBudget::default()
            }
        );
    }

    fn file(path: &str, removable: bool) -> Candidate {
        Candidate {
            path: path.into(),
            size: 10,
            removable,
        }
    }

    fn group(last_used: f64, lighten: &[&str], rest: &[&str]) -> Group {
        let files = |paths: &[&str]| paths.iter().map(|path| file(path, true)).collect();
        Group {
            last_used,
            lighten: files(lighten),
            rest: files(rest),
        }
    }

    fn planned(groups: Vec<Group>, need: u64) -> Vec<Vec<String>> {
        plan(groups, need)
            .into_iter()
            .map(|unit| unit.into_iter().map(|file| file.path).collect())
            .collect()
    }

    #[test]
    fn plans_the_oldest_first_and_stops_at_what_is_needed() {
        let groups = vec![
            group(3.0, &[], &["new"]),
            group(1.0, &[], &["old"]),
            group(2.0, &[], &["middle"]),
        ];
        assert_eq!(planned(groups.clone(), 15), [["old"], ["middle"]]);
        assert_eq!(planned(groups.clone(), 10), [["old"]]);
        assert!(planned(groups, 0).is_empty());
    }

    #[test]
    fn plans_raw_files_and_other_frames_ahead_of_whole_groups() {
        let groups = vec![
            group(1.0, &["old.raw", "old-2"], &["old", "old.mov"]),
            group(2.0, &["new.raw"], &["new"]),
        ];
        assert_eq!(
            planned(groups.clone(), 30),
            [["old.raw"], ["old-2"], ["new.raw"]]
        );
        let all = planned(groups, 1000);
        assert_eq!(all[3], ["old", "old.mov"]);
        assert_eq!(all[4], ["new"]);
    }

    #[test]
    fn a_group_goes_whole_or_not_at_all() {
        let mut pinned_frame = group(1.0, &["a-2", "a-3"], &["a"]);
        pinned_frame.lighten[1].removable = false;
        let mut pinned_video = group(2.0, &[], &["b", "b.mov"]);
        pinned_video.rest[1].removable = false;
        let free = group(3.0, &[], &["c"]);
        assert_eq!(
            planned(vec![pinned_frame, pinned_video, free], 1000),
            [vec!["a-2"], vec!["c"]]
        );
    }

    fn sources(synced: &Synced) -> Vec<DownloadSource> {
        vec![DownloadSource {
            id: synced.id,
            settings: synced.settings.clone(),
        }]
    }

    /// A budget that wants everything gone: only what's kept from going
    /// stays.
    const EVERYTHING: Volume = Volume {
        total: 1000 * GB,
        free: 0,
    };

    fn remove(synced: &Synced, budget: SpaceBudget, volume: Volume) -> FreeUpSummary {
        synced
            .library
            .remove_to_budget(&synced.plugins, &sources(synced), &budget, volume)
            .unwrap()
    }

    fn here(synced: &Synced) -> Vec<bool> {
        photos_of(synced).iter().map(|p| p.path.is_file()).collect()
    }

    fn photos_of(synced: &Synced) -> Vec<Photo> {
        synced.library.photos().unwrap()
    }

    fn size_of(photo: &Photo) -> u64 {
        std::fs::metadata(&photo.path).unwrap().len()
    }

    #[test]
    fn nothing_goes_until_the_catalog_is_synced() {
        let synced = synced("free-gate", 2);
        let budget = SpaceBudget {
            cap_percent: 1,
            floor_gb: 1_000_000,
            copies: 1,
        };
        let summary = synced
            .library
            .free_up_space(&synced.plugins, &sources(&synced), &budget)
            .unwrap();
        assert_eq!(summary.removed, 0);
        assert!(summary.stopped.unwrap().contains("catalog"));
        assert_eq!(here(&synced), [true, true]);
        assert!(synced.s3.take().is_empty());
        test_support::remove(&synced.root);
    }

    #[test]
    fn does_not_touch_the_target_when_within_budget() {
        let synced = synced("free-within", 2);
        let roomy = Volume {
            total: 1000 * GB,
            free: 500 * GB,
        };
        let summary = remove(&synced, SpaceBudget::default(), roomy);
        assert_eq!(summary, FreeUpSummary::default());
        assert_eq!(here(&synced), [true, true]);
        assert!(synced.s3.take().is_empty());
        test_support::remove(&synced.root);
    }

    #[test]
    fn removes_what_was_wanted_longest_ago_down_to_the_mark() {
        let synced = synced("free-oldest", 3);
        let photos = photos_of(&synced);
        // Opening the oldest makes it the last to go.
        synced.library.touch(&photos[0]).unwrap();
        let used: u64 = photos.iter().map(size_of).sum();
        // Originals fill the volume by exactly a little too much: the cap's
        // 100%, and a tenth of it is under one photo.
        let volume = Volume {
            total: used - 1,
            free: u64::MAX,
        };
        let budget = SpaceBudget {
            cap_percent: 100,
            floor_gb: 0,
            copies: 1,
        };
        let summary = remove(&synced, budget, volume);
        assert_eq!(summary.removed, 1, "{summary:?}");
        assert_eq!(summary.bytes, size_of_gone(&synced, &photos));
        assert_eq!(summary.short, 0);
        assert_eq!(here(&synced), [true, false, true]);
        // It's no longer counted, and comes back when it's opened.
        let usage = synced.library.originals_usage().unwrap();
        assert_eq!(usage.files, 2);
        synced
            .library
            .fetch_original(
                &synced.plugins,
                &sources(&synced),
                &photos[1],
                &Default::default(),
            )
            .unwrap();
        assert_eq!(here(&synced), [true, true, true]);
        test_support::remove(&synced.root);
    }

    /// The recorded size of the one original that's gone.
    fn size_of_gone(synced: &Synced, photos: &[Photo]) -> u64 {
        let items = synced.library.sync_items(synced.id).unwrap();
        let gone = photos.iter().find(|photo| !photo.path.is_file()).unwrap();
        let path = synced.library.relative(&gone.path).unwrap();
        items[&path].size.unwrap()
    }

    #[test]
    fn keeps_what_is_pinned_and_still_counts_it() {
        let synced = synced("free-pinned", 3);
        let photos = photos_of(&synced);
        synced.library.pin_photo(&photos[0]).unwrap();
        let summary = remove(&synced, SpaceBudget::default(), EVERYTHING);
        assert_eq!(summary.removed, 2);
        assert!(summary.short > 0);
        assert_eq!(here(&synced), [true, false, false]);
        let usage = synced.library.originals_usage().unwrap();
        assert_eq!(usage.bytes, size_of(&photos[0]));
        assert_eq!(usage.pinned_bytes, size_of(&photos[0]));
        test_support::remove(&synced.root);
    }

    /// Replaces a target's record of a photo's file.
    fn set_record(synced: &Synced, photo: &Photo, change: impl FnOnce(&mut SyncItemRecord)) {
        let path = synced.library.relative(&photo.path).unwrap();
        let mut item = synced.library.sync_items(synced.id).unwrap()[&path].clone();
        change(&mut item);
        synced
            .library
            .catalog
            .record_sync(synced.id, &item)
            .unwrap();
    }

    #[test]
    fn keeps_what_is_not_confirmed() {
        let synced = synced("free-unconfirmed", 3);
        let photos = photos_of(&synced);
        // Its upload failed, and another's copy is of something else.
        set_record(&synced, &photos[0], |item| item.state = SyncState::Failed);
        set_record(&synced, &photos[1], |item| item.size = Some(1));
        remove(&synced, SpaceBudget::default(), EVERYTHING);
        assert_eq!(here(&synced), [true, true, false]);
        test_support::remove(&synced.root);
    }

    #[test]
    fn keeps_everything_while_the_target_cannot_be_reached() {
        let synced = synced("free-unreachable", 2);
        synced.s3.fail_next(&[503]);
        let summary = remove(&synced, SpaceBudget::default(), EVERYTHING);
        assert_eq!(summary.removed, 0);
        assert!(summary.stopped.unwrap().contains("reached"));
        assert_eq!(here(&synced), [true, true]);

        // Nor while its settings are known to be wrong.
        let library = &synced.library;
        let error = Some("Access denied.");
        library
            .catalog
            .record_device_sync(Some(synced.id), false, error, None)
            .unwrap();
        assert_eq!(
            remove(&synced, SpaceBudget::default(), EVERYTHING).removed,
            0
        );
        library
            .catalog
            .record_device_sync(Some(synced.id), true, None, None)
            .unwrap();

        // Once it's back, they can go.
        assert_eq!(
            remove(&synced, SpaceBudget::default(), EVERYTHING).removed,
            2
        );
        test_support::remove(&synced.root);
    }

    #[test]
    fn needs_as_many_copies_as_asked_for() {
        let synced = synced("free-copies", 2);
        let budget = SpaceBudget {
            copies: 2,
            ..SpaceBudget::default()
        };
        assert_eq!(remove(&synced, budget, EVERYTHING).removed, 0);
        assert_eq!(here(&synced), [true, true]);

        // A second target with only one of the photos.
        let library = &synced.library;
        let second = library.add_sync_target("target-2").unwrap();
        let photos = photos_of(&synced);
        let path = library.relative(&photos[0].path).unwrap();
        let item = library.sync_items(synced.id).unwrap()[&path].clone();
        library.catalog.record_sync(second, &item).unwrap();
        let mut both = sources(&synced);
        both.push(DownloadSource {
            id: second,
            settings: synced.settings.clone(),
        });
        let summary = library
            .remove_to_budget(&synced.plugins, &both, &budget, EVERYTHING)
            .unwrap();
        assert_eq!(summary.removed, 1);
        assert_eq!(here(&synced), [false, true]);

        // A copy on a target that can't be reached doesn't count.
        let summary = library
            .remove_to_budget(&synced.plugins, &both[..1], &budget, EVERYTHING)
            .unwrap();
        assert_eq!(summary.removed, 0);
        test_support::remove(&synced.root);
    }

    #[test]
    fn keeps_a_photo_with_no_thumbnail_to_show() {
        let synced = synced("free-thumbnail", 2);
        let photos = photos_of(&synced);
        let thumbnail = photos[0].thumbnail.as_ref().unwrap();
        std::fs::remove_file(thumbnail).unwrap();
        remove(&synced, SpaceBudget::default(), EVERYTHING);
        assert_eq!(here(&synced), [true, false]);
        test_support::remove(&synced.root);
    }

    #[test]
    fn stops_if_the_library_is_not_there() {
        let synced = synced("free-unplugged", 2);
        let marker: PathBuf = synced.library.info.path.join(super::super::MARKER_FILE);
        let root = &synced.library.info.path;
        protect::set_writable(root, true).unwrap();
        std::fs::remove_file(marker).unwrap();
        let removed = synced.library.remove_to_budget(
            &synced.plugins,
            &sources(&synced),
            &SpaceBudget::default(),
            EVERYTHING,
        );
        assert!(removed.is_err());
        assert_eq!(here(&synced), [true, true]);
        test_support::remove(&synced.root);
    }

    /// A photo of the library at `name`, with no file on the disk.
    fn shot(root: &Path, name: &str, taken: f64, thumbnail: Option<PathBuf>) -> Photo {
        Photo {
            path: root.join(format!("Originals/{name}.JPG")),
            raw_pair: None,
            live_video: None,
            thumbnail,
            width: 1,
            height: 1,
            meta: photos::CaptureMeta {
                captured_at: Some(taken),
                ..Default::default()
            },
        }
    }

    #[test]
    fn groups_a_stack_by_what_can_go_first() {
        let synced = synced("free-groups", 1);
        let library = &synced.library;
        let root = library.info.path.clone();
        let thumbnail = photos_of(&synced)[0].thumbnail.clone();
        assert!(thumbnail.as_ref().unwrap().is_file());

        let mut cover = shot(&root, "cover", 100.0, thumbnail.clone());
        cover.raw_pair = Some(root.join("Originals/cover.ARW"));
        cover.live_video = Some(root.join("Originals/cover.MOV"));
        let mut frame = shot(&root, "frame", 90.0, thumbnail);
        frame.raw_pair = Some(root.join("Originals/frame.ARW"));
        // Nothing to show for it, though its raw file can still go.
        let mut bare = shot(&root, "bare", 80.0, None);
        bare.raw_pair = Some(root.join("Originals/bare.ARW"));
        let stack = Stack {
            kind: photos::StackKind::Burst,
            photos: vec![frame, cover, bare],
            cover: 1,
        };

        let names = [
            "cover.JPG",
            "cover.ARW",
            "cover.MOV",
            "frame.JPG",
            "frame.ARW",
            "bare.JPG",
            "bare.ARW",
        ];
        let paths = names.map(|name| format!("Originals/{name}"));
        let originals = Originals {
            here: paths.iter().map(|path| (path.clone(), 10)).collect(),
            // Opening any frame counts for the group.
            opened: [("Originals/bare.JPG".to_owned(), 500.0)].into(),
            pinned: ["Originals/frame.ARW".to_owned()].into(),
            copies: paths.iter().map(|path| (path.clone(), 1)).collect(),
            required: 1,
        };
        let group = library.group(&stack, &originals).unwrap();
        assert_eq!(group.last_used, 500.0);
        let list = |files: &[Candidate]| -> Vec<(String, bool)> {
            files
                .iter()
                .map(|file| (file.path.replace("Originals/", ""), file.removable))
                .collect()
        };
        let expected = |files: &[(&str, bool)]| -> Vec<(String, bool)> {
            files.iter().map(|(n, r)| (n.to_string(), *r)).collect()
        };
        assert_eq!(
            list(&group.lighten),
            expected(&[
                ("frame.ARW", false),
                ("cover.ARW", true),
                ("bare.ARW", true),
                ("frame.JPG", true),
                ("bare.JPG", false),
            ])
        );
        assert_eq!(
            list(&group.rest),
            expected(&[("cover.JPG", true), ("cover.MOV", true)])
        );
        test_support::remove(&synced.root);
    }

    #[test]
    fn keeps_a_target_that_holds_the_only_copy() {
        let synced = synced("free-strand", 2);
        let library = &synced.library;
        let handle = LibraryHandle {
            info: library.info.clone(),
            catalog_path: PathBuf::new(),
            open: Some(library.clone()),
        };
        handle.check_sync_target_forgettable("target-1").unwrap();
        assert_eq!(
            remove(&synced, SpaceBudget::default(), EVERYTHING).removed,
            2
        );
        let error = handle
            .check_sync_target_forgettable("target-1")
            .unwrap_err();
        assert!(error.to_string().contains("only on this target"));
        assert!(library.remove_sync_target(synced.id).is_err());
        // Other targets aren't held back by it.
        handle.check_sync_target_forgettable("target-2").unwrap();
        test_support::remove(&synced.root);
    }

    #[test]
    fn overview_says_what_is_pinned() {
        let synced = synced("free-overview", 2);
        let library = &synced.library;
        let photos = photos_of(&synced);
        library.pin_photo(&photos[0]).unwrap();
        let album: Id = library.create_album("Keep", None).unwrap();
        library.pin_album(album).unwrap();
        let overview = library.space_overview().unwrap();
        assert_eq!(overview.pinned_photos, 1);
        assert_eq!(overview.pinned_albums, ["Keep"]);
        assert_eq!(overview.usage.pinned_bytes, size_of(&photos[0]));
        assert!(overview.volume.is_some_and(|volume| volume.total > 0));
        test_support::remove(&synced.root);
    }
}
