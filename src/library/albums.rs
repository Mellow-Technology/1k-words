//! Albums: named sets of a library's photos, kept in its catalog. An album
//! can hold other albums as well as photos, so they nest, like folders. A
//! photo can be in any number of albums.
//!
//! Unlike the rest of the catalog, albums can't be found again from the
//! originals, and the catalog is kept in the app's config directory, apart
//! from them. So a copy is kept in the library's folder too, rewritten as
//! they change (see `save_albums`), and the albums are restored from it
//! into a catalog that has none.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::Library;
use super::catalog::AlbumConflict;
use super::id::Id;
use super::protect::Unlocked;
use crate::photos::Photo;

/// The folder in a library for what the app records about its photos, kept
/// with them.
pub const CATALOG_DIR: &str = "Catalog";
/// The copy of the albums in `CATALOG_DIR`.
const ALBUMS_FILE: &str = "albums.json";

pub type AlbumId = Id;

/// How long deleted albums are kept, in case another device changed them
/// meanwhile (see `Catalog::album_conflicts`).
const KEEP_DELETED_ALBUMS_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    pub id: AlbumId,
    /// The album it's in, if any.
    pub parent: Option<AlbumId>,
    pub name: String,
    /// How many photos are in it (not counting the albums in it).
    pub photos: usize,
    /// Whether it shows the photos of the albums in it as well as its own.
    pub show_nested: bool,
}

impl Library {
    /// Every album, by name.
    pub fn albums(&self) -> Result<Vec<Album>> {
        self.catalog.albums()
    }

    /// The photos an album shows, oldest first: its own, and those of the
    /// albums in it if it shows them.
    pub fn album_photos(&self, album: AlbumId) -> Result<Vec<Photo>> {
        let shown = shown_albums(&self.albums()?, album);
        Ok(self.photos_from(self.catalog.album_photos(&shown)?))
    }

    /// Makes an album called `name`, in `parent` if given.
    pub fn create_album(&self, name: &str, parent: Option<AlbumId>) -> Result<AlbumId> {
        let album = self.catalog.create_album(album_name(name)?, parent)?;
        self.save_albums();
        Ok(album)
    }

    pub fn rename_album(&self, album: AlbumId, name: &str) -> Result<()> {
        self.catalog.rename_album(album, album_name(name)?)?;
        self.save_albums();
        Ok(())
    }

    /// Puts an album, with the albums in it, in `parent`, or at the top
    /// level.
    pub fn move_album(&self, album: AlbumId, parent: Option<AlbumId>) -> Result<()> {
        let albums = self.albums()?;
        if !albums.iter().any(|a| a.id == album) {
            bail!("the album isn't there any more");
        }
        if let Some(parent) = parent {
            if !albums.iter().any(|a| a.id == parent) {
                bail!("the album to put it in isn't there any more");
            }
            if nested_albums(&albums, album).contains(&parent) {
                bail!("an album can't go inside itself");
            }
        }
        self.catalog.move_album(album, parent)?;
        self.save_albums();
        Ok(())
    }

    /// Sets whether an album shows the photos of the albums in it.
    pub fn set_album_show_nested(&self, album: AlbumId, show: bool) -> Result<()> {
        self.catalog.set_album_show_nested(album, show)?;
        self.save_albums();
        Ok(())
    }

    /// Adds photos from the library to an album, returning how many
    /// weren't in it already.
    pub fn add_to_album(&self, album: AlbumId, photos: &[Photo]) -> Result<usize> {
        let paths = photos
            .iter()
            .map(|photo| self.relative(&photo.path))
            .collect::<Result<Vec<_>>>()?;
        let added = self.catalog.add_paths_to_album(album, &paths)?;
        self.save_albums();
        Ok(added)
    }

    /// Takes photos out of an album (they stay in the library), returning
    /// how many were in it. If it shows the photos of the albums in it,
    /// they're taken out of those too, so it no longer shows them.
    pub fn remove_from_album(&self, album: AlbumId, photos: &[Photo]) -> Result<usize> {
        let paths = photos
            .iter()
            .map(|photo| self.relative(&photo.path))
            .collect::<Result<Vec<_>>>()?;
        let shown = shown_albums(&self.albums()?, album);
        let removed = self.catalog.remove_paths_from_albums(&shown, &paths)?;
        self.save_albums();
        Ok(removed)
    }

    /// The albums a photo is in; none if it isn't in the library.
    pub fn albums_with(&self, photo: &Photo) -> Result<Vec<AlbumId>> {
        match self.relative(&photo.path) {
            Ok(path) => self.catalog.albums_with(&path),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Deletes an album and every album in it. Their photos stay in the
    /// library.
    pub fn delete_album(&self, album: AlbumId) -> Result<()> {
        self.catalog
            .delete_albums(&nested_albums(&self.albums()?, album))?;
        self.save_albums();
        Ok(())
    }

    /// Albums deleted on one device but changed on another since, for the
    /// user to say which they meant.
    pub fn album_conflicts(&self) -> Result<Vec<AlbumConflict>> {
        self.catalog.album_conflicts()
    }

    /// Keeps an album in conflict, or lets it go.
    pub fn resolve_album_conflict(&self, album: AlbumId, keep: bool) -> Result<()> {
        self.catalog.resolve_album_conflict(album, keep)?;
        self.save_albums();
        Ok(())
    }

    /// Changes an album as if on another device, `device`, and synced here.
    #[cfg(test)]
    pub fn change_album_elsewhere(&self, album: AlbumId, device: &str) {
        self.catalog
            .change_album_elsewhere(album, Id::new(), device);
    }

    /// Removes albums deleted long enough ago that every device has had the
    /// chance to say it changed them.
    pub(super) fn purge_deleted_albums(&self) -> Result<()> {
        self.catalog
            .purge_deleted_albums(super::now() - KEEP_DELETED_ALBUMS_SECS)
    }

    /// Writes the copy of the albums in the library's folder. Failing to
    /// is reported, but isn't an error: the albums are in the catalog.
    pub(super) fn save_albums(&self) {
        if let Err(error) = self.try_save_albums() {
            eprintln!("couldn't keep a copy of the albums: {error:#}");
        }
    }

    fn try_save_albums(&self) -> Result<()> {
        // So a copy read earlier can't be written over a later one.
        let _saving = self.saving_albums.lock().unwrap_or_else(|e| e.into_inner());
        let albums = self.albums()?;
        let path = self.info.path.join(CATALOG_DIR).join(ALBUMS_FILE);
        if albums.is_empty() && !path.exists() {
            return Ok(());
        }
        let mut photos: HashMap<AlbumId, Vec<String>> = HashMap::new();
        for (album, path) in self.catalog.album_memberships()? {
            photos.entry(album).or_default().push(path);
        }
        let copy = AlbumsFile {
            albums: tree_order(&albums)
                .into_iter()
                .map(|(_, album)| SavedAlbum {
                    id: SavedId::Id(album.id.to_string()),
                    parent: album.parent.map(|parent| SavedId::Id(parent.to_string())),
                    name: album.name.clone(),
                    show_nested: album.show_nested,
                    photos: photos.remove(&album.id).unwrap_or_default(),
                })
                .collect(),
        };
        self.write_catalog_file(ALBUMS_FILE, &serde_json::to_vec_pretty(&copy)?)
    }

    /// Writes a file in `CATALOG_DIR`, making the folder if need be.
    /// Written beside it then renamed, so it's never half written.
    fn write_catalog_file(&self, name: &str, contents: &[u8]) -> Result<()> {
        let root = &self.info.path;
        if !root.is_dir() {
            bail!("{} isn't there", root.display());
        }
        let dir = root.join(CATALOG_DIR);
        if !dir.is_dir() {
            // The library's folder is read-only; the folder in it isn't.
            let mut unlocked = Unlocked::new(self.info.read_only);
            unlocked.unlock(root)?;
            std::fs::create_dir(&dir).or_else(|error| match dir.is_dir() {
                true => Ok(()),
                false => Err(error),
            })?;
            unlocked.relock()?;
        }
        let partial = dir.join(format!(".{}.partial", uuid::Uuid::new_v4()));
        let write = || -> Result<()> {
            let mut file = std::fs::File::create(&partial)?;
            file.write_all(contents)?;
            file.sync_all()?;
            std::fs::rename(&partial, dir.join(name))?;
            Ok(())
        };
        write().inspect_err(|_| {
            std::fs::remove_file(&partial).ok();
        })
    }

    /// Restores the albums from the copy in the library's folder if the
    /// catalog has none, as when it's been lost. Photos not in the catalog
    /// can't be added to them; if any aren't, the copy is kept as it was,
    /// beside the new one, under a name with the time in it.
    pub(super) fn restore_albums(&self) -> Result<()> {
        let path = self.info.path.join(CATALOG_DIR).join(ALBUMS_FILE);
        if !self.albums()?.is_empty() || !path.is_file() {
            return Ok(());
        }
        let contents = std::fs::read(&path)?;
        let copy: AlbumsFile = serde_json::from_slice(&contents)
            .with_context(|| format!("reading {}", path.display()))?;
        if copy.albums.is_empty() {
            return Ok(());
        }
        // Parents come before the albums in them.
        let mut ids = HashMap::new();
        let mut missing = 0;
        for album in &copy.albums {
            let parent = album
                .parent
                .as_ref()
                .and_then(|parent| ids.get(parent).copied());
            let id = self.catalog.create_album(&album.name, parent)?;
            ids.insert(album.id.clone(), id);
            if album.show_nested {
                self.catalog.set_album_show_nested(id, true)?;
            }
            for photo in &album.photos {
                if self
                    .catalog
                    .add_paths_to_album(id, std::slice::from_ref(photo))
                    .is_err()
                {
                    missing += 1;
                }
            }
        }
        if missing > 0 {
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            self.write_catalog_file(&format!("albums-{time}.json"), &contents)?;
        }
        self.try_save_albums()
    }
}

/// The copy of a library's albums kept in its folder.
#[derive(Serialize, Deserialize)]
struct AlbumsFile {
    /// Each before the albums in it.
    albums: Vec<SavedAlbum>,
}

#[derive(Serialize, Deserialize)]
struct SavedAlbum {
    /// Its id in the catalog it was saved from.
    id: SavedId,
    parent: Option<SavedId>,
    name: String,
    #[serde(default)]
    show_nested: bool,
    /// Its photos' displayed files, relative to the library's folder.
    photos: Vec<String>,
}

/// An album's id in a saved copy: a UUID, or a number from before they
/// were (schema 7 and earlier). Only used to find albums' parents.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
enum SavedId {
    Number(i64),
    Id(String),
}

/// `name`, trimmed, if it's a name.
fn album_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("an album needs a name");
    }
    Ok(name)
}

/// The album with `id` and every album in it, however deeply.
pub fn nested_albums(albums: &[Album], id: AlbumId) -> Vec<AlbumId> {
    let mut nested = vec![id];
    let mut ix = 0;
    while let Some(&parent) = nested.get(ix) {
        for child in child_albums(albums, Some(parent)) {
            // Guard against a loop, which the catalog shouldn't have.
            if !nested.contains(&child.id) {
                nested.push(child.id);
            }
        }
        ix += 1;
    }
    nested
}

/// The albums whose photos `album` shows: it, and if it shows them, the
/// albums in it.
pub fn shown_albums(albums: &[Album], album: AlbumId) -> Vec<AlbumId> {
    match albums.iter().find(|a| a.id == album) {
        Some(a) if a.show_nested => nested_albums(albums, album),
        _ => vec![album],
    }
}

/// The albums in `parent` (or at the top level), in the order given.
pub fn child_albums(albums: &[Album], parent: Option<AlbumId>) -> impl Iterator<Item = &Album> {
    albums.iter().filter(move |album| album.parent == parent)
}

/// The album with `id` and the albums it's in, outermost first.
pub fn album_path(albums: &[Album], id: AlbumId) -> Vec<&Album> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut next = Some(id);
    while let Some(id) = next
        && seen.insert(id)
        && let Some(album) = albums.iter().find(|album| album.id == id)
    {
        path.push(album);
        next = album.parent;
    }
    path.reverse();
    path
}

/// Every album, each followed by those in it, with how deeply it's nested
/// (0 at the top level).
pub fn tree_order(albums: &[Album]) -> Vec<(usize, &Album)> {
    fn visit<'a>(
        albums: &'a [Album],
        parent: Option<AlbumId>,
        depth: usize,
        ordered: &mut Vec<(usize, &'a Album)>,
    ) {
        for album in child_albums(albums, parent) {
            // Guard against a loop, which the catalog shouldn't have.
            if ordered.iter().any(|(_, seen)| seen.id == album.id) {
                continue;
            }
            ordered.push((depth, album));
            visit(albums, Some(album.id), depth + 1, ordered);
        }
    }
    let mut ordered = Vec::new();
    visit(albums, None, 0, &mut ordered);
    ordered
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::super::{LibraryInfo, test_support};
    use super::*;

    fn album(id: u128, parent: Option<u128>, name: &str) -> Album {
        Album {
            id: Id::from_u128(id),
            parent: parent.map(Id::from_u128),
            name: name.into(),
            photos: 0,
            show_nested: false,
        }
    }

    #[test]
    fn walks_the_tree() {
        let albums = [
            album(1, None, "Family"),
            album(2, Some(3), "Japan"),
            album(3, None, "Trips"),
            album(4, Some(2), "Kyoto"),
            album(5, Some(3), "Peru"),
        ];
        let names = |albums: Vec<&Album>| albums.iter().map(|a| a.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(album_path(&albums, Id::from_u128(4))),
            ["Trips", "Japan", "Kyoto"]
        );
        assert!(album_path(&albums, Id::from_u128(99)).is_empty());
        assert_eq!(
            names(child_albums(&albums, Some(Id::from_u128(3))).collect()),
            ["Japan", "Peru"]
        );
        let tree: Vec<_> = tree_order(&albums)
            .into_iter()
            .map(|(depth, album)| (depth, album.name.as_str()))
            .collect();
        assert_eq!(
            tree,
            [
                (0, "Family"),
                (0, "Trips"),
                (1, "Japan"),
                (2, "Kyoto"),
                (1, "Peru")
            ]
        );
    }

    #[test]
    fn reads_copies_saved_before_ids_were_uuids() {
        let old = r#"{"albums": [
            {"id": 1, "parent": null, "name": "Trips", "photos": []},
            {"id": 2, "parent": 1, "name": "Japan", "show_nested": true, "photos": ["a.jpg"]}
        ]}"#;
        let copy: AlbumsFile = serde_json::from_str(old).unwrap();
        assert!(copy.albums[1].parent == Some(copy.albums[0].id.clone()));
        let new = serde_json::to_string(&SavedAlbum {
            id: SavedId::Id(Id::new().to_string()),
            parent: None,
            name: "Peru".into(),
            show_nested: false,
            photos: Vec::new(),
        })
        .unwrap();
        let saved: SavedAlbum = serde_json::from_str(&new).unwrap();
        assert!(matches!(saved.id, SavedId::Id(_)));
    }

    #[test]
    fn makes_and_deletes_albums() {
        let (config_dir, library_dir) = test_support::temp_dirs("albums");
        let library = Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: "albums".into(),
                    name: "Test".into(),
                    path: library_dir,
                    read_only: true,
                },
                &config_dir,
            )
            .unwrap(),
        );
        assert!(library.create_album("  ", None).is_err());
        let trips = library.create_album(" Trips ", None).unwrap();
        let japan = library.create_album("Japan", Some(trips)).unwrap();
        library.create_album("Kyoto", Some(japan)).unwrap();
        let family = library.create_album("Family", None).unwrap();
        library.rename_album(family, "Our Family").unwrap();
        assert!(library.rename_album(family, "").is_err());
        let names = |library: &Library| {
            library
                .albums()
                .unwrap()
                .into_iter()
                .map(|album| album.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&library), ["Japan", "Kyoto", "Our Family", "Trips"]);

        // Importing into an album adds the photos to it, and those already
        // in the library too.
        let card = config_dir.parent().unwrap().join("card");
        std::fs::create_dir_all(&card).unwrap();
        for ix in 0..2u8 {
            image::RgbImage::from_pixel(40, 30, image::Rgb([ix * 90, 60, 120]))
                .save(card.join(format!("IMG_{ix}.JPG")))
                .unwrap();
        }
        let summary = library
            .import_to_album(&[card.join("IMG_0.JPG")], japan)
            .unwrap();
        assert_eq!(summary.imported, 1);
        let summary = library.import_to_album(&[card.clone()], japan).unwrap();
        assert_eq!((summary.imported, summary.duplicates), (1, 1));
        let summary = library.import_to_album(&[card.clone()], family).unwrap();
        assert_eq!(summary.duplicates, 2);
        assert_eq!(library.album_photos(japan).unwrap().len(), 2);
        assert_eq!(library.album_photos(family).unwrap().len(), 2);
        assert_eq!(library.photos().unwrap().len(), 2);
        let counts: Vec<_> = library
            .albums()
            .unwrap()
            .into_iter()
            .map(|album| (album.name, album.photos))
            .collect();
        assert_eq!(counts[0], ("Japan".into(), 2));

        // Photos already in the library can be added too.
        let photos = library.photos().unwrap();
        assert_eq!(library.add_to_album(trips, &photos).unwrap(), 2);
        assert_eq!(library.add_to_album(trips, &photos[..1]).unwrap(), 0);
        assert_eq!(library.album_photos(trips).unwrap().len(), 2);

        // By id, as they were made.
        assert_eq!(
            library.albums_with(&photos[0]).unwrap(),
            [trips, japan, family]
        );
        assert_eq!(library.remove_from_album(trips, &photos[..1]).unwrap(), 1);
        assert_eq!(library.album_photos(trips).unwrap().len(), 1);
        assert_eq!(library.albums_with(&photos[0]).unwrap(), [japan, family]);

        // Deleting an album deletes those in it.
        library.delete_album(trips).unwrap();
        assert_eq!(names(&library), ["Our Family"]);
        test_support::remove(config_dir.parent().unwrap());
    }

    fn open_library(name: &str, config_dir: &std::path::Path, path: PathBuf) -> Arc<Library> {
        Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: name.into(),
                    name: "Test".into(),
                    path,
                    read_only: true,
                },
                config_dir,
            )
            .unwrap(),
        )
    }

    /// A library with three photos: two in Kyoto, inside Japan, inside
    /// Trips, and one in Japan.
    fn library_with_albums(name: &str) -> (PathBuf, PathBuf, Arc<Library>, [AlbumId; 3]) {
        let (config_dir, library_dir) = test_support::temp_dirs(name);
        let library = open_library(name, &config_dir, library_dir.clone());
        let trips = library.create_album("Trips", None).unwrap();
        let japan = library.create_album("Japan", Some(trips)).unwrap();
        let kyoto = library.create_album("Kyoto", Some(japan)).unwrap();
        let card = config_dir.parent().unwrap().join("card");
        std::fs::create_dir_all(&card).unwrap();
        for ix in 0..3u8 {
            image::RgbImage::from_pixel(40, 30, image::Rgb([ix * 80, 60, 120]))
                .save(card.join(format!("IMG_{ix}.JPG")))
                .unwrap();
        }
        let shots: Vec<PathBuf> = (0..3)
            .map(|ix| card.join(format!("IMG_{ix}.JPG")))
            .collect();
        library.import_to_album(&shots[..2], kyoto).unwrap();
        library.import_to_album(&shots[2..], japan).unwrap();
        (config_dir, library_dir, library, [trips, japan, kyoto])
    }

    #[test]
    fn moves_albums_and_shows_those_inside() {
        let (config_dir, _, library, [trips, japan, kyoto]) = library_with_albums("albums-nested");
        let parent = |album: AlbumId| {
            library
                .albums()
                .unwrap()
                .into_iter()
                .find(|a| a.id == album)
                .unwrap()
                .parent
        };

        // Not into itself, or an album in it.
        assert!(library.move_album(trips, Some(trips)).is_err());
        assert!(library.move_album(trips, Some(kyoto)).is_err());
        assert!(library.move_album(trips, Some(Id::new())).is_err());
        library.move_album(kyoto, Some(trips)).unwrap();
        assert_eq!(parent(kyoto), Some(trips));
        library.move_album(kyoto, None).unwrap();
        assert_eq!(parent(kyoto), None);
        library.move_album(kyoto, Some(japan)).unwrap();

        // Trips has no photos of its own, until it shows those inside.
        assert!(library.album_photos(trips).unwrap().is_empty());
        library.set_album_show_nested(trips, true).unwrap();
        let shown = library.album_photos(trips).unwrap();
        assert_eq!(shown.len(), 3);
        // Taking one out of it takes it out of the albums inside.
        assert_eq!(library.remove_from_album(trips, &shown[..1]).unwrap(), 1);
        assert_eq!(library.album_photos(trips).unwrap().len(), 2);
        library.set_album_show_nested(trips, false).unwrap();
        assert!(library.album_photos(trips).unwrap().is_empty());
        test_support::remove(config_dir.parent().unwrap());
    }

    #[test]
    fn restores_albums_from_the_copy_in_the_library() {
        let (config_dir, library_dir, library, [trips, _, _]) =
            library_with_albums("albums-restore");
        library.set_album_show_nested(trips, true).unwrap();
        let copy = library_dir.join(CATALOG_DIR).join(ALBUMS_FILE);
        assert!(copy.is_file());
        let saved = |library: &Library| {
            let albums = library.albums().unwrap();
            tree_order(&albums)
                .into_iter()
                .map(|(depth, album)| {
                    let photos = library.catalog.album_photos(&[album.id]).unwrap().len();
                    (depth, album.name.clone(), album.show_nested, photos)
                })
                .collect::<Vec<_>>()
        };
        let before = saved(&library);
        assert_eq!(
            before,
            [
                (0, "Trips".into(), true, 0),
                (1, "Japan".into(), false, 1),
                (2, "Kyoto".into(), false, 2),
            ]
        );

        // With the albums gone from the catalog, but not the photos, all of
        // them come back.
        let ids: Vec<AlbumId> = library.albums().unwrap().iter().map(|a| a.id).collect();
        library.catalog.delete_albums(&ids).unwrap();
        drop(library);
        let library = open_library("albums-restore", &config_dir, library_dir.clone());
        assert_eq!(saved(&library), before);
        let kept = |dir: &std::path::Path| {
            std::fs::read_dir(dir.join(CATALOG_DIR))
                .unwrap()
                .filter(|entry| {
                    let name = entry.as_ref().unwrap().file_name();
                    name.to_string_lossy().starts_with("albums-")
                })
                .count()
        };
        assert_eq!(kept(&library_dir), 0);

        // With the whole catalog gone, the albums come back without their
        // photos, and the copy's kept as it was.
        drop(library);
        std::fs::remove_file(super::super::catalog_path(&config_dir, "albums-restore")).unwrap();
        let library = open_library("albums-restore", &config_dir, library_dir.clone());
        assert_eq!(
            saved(&library),
            [
                (0, "Trips".into(), true, 0),
                (1, "Japan".into(), false, 0),
                (2, "Kyoto".into(), false, 0),
            ]
        );
        assert_eq!(kept(&library_dir), 1);
        test_support::remove(config_dir.parent().unwrap());
    }
}
