//! Albums: named sets of a library's photos, kept in its catalog. An album
//! can hold other albums as well as photos, so they nest, like folders. A
//! photo can be in any number of albums.

use std::collections::HashSet;

use anyhow::{Result, bail};

use super::Library;
use crate::photos::Photo;

pub type AlbumId = i64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    pub id: AlbumId,
    /// The album it's in, if any.
    pub parent: Option<AlbumId>,
    pub name: String,
    /// How many photos are in it (not counting the albums in it).
    pub photos: usize,
}

impl Library {
    /// Every album, by name.
    pub fn albums(&self) -> Result<Vec<Album>> {
        self.catalog.albums()
    }

    /// The photos in an album (not the albums in it), oldest first.
    pub fn album_photos(&self, album: AlbumId) -> Result<Vec<Photo>> {
        Ok(self.photos_from(self.catalog.album_photos(album)?))
    }

    /// Makes an album called `name`, in `parent` if given.
    pub fn create_album(&self, name: &str, parent: Option<AlbumId>) -> Result<AlbumId> {
        self.catalog.create_album(album_name(name)?, parent)
    }

    pub fn rename_album(&self, album: AlbumId, name: &str) -> Result<()> {
        self.catalog.rename_album(album, album_name(name)?)
    }

    /// Adds photos from the library to an album, returning how many
    /// weren't in it already.
    pub fn add_to_album(&self, album: AlbumId, photos: &[Photo]) -> Result<usize> {
        let paths = photos
            .iter()
            .map(|photo| self.relative(&photo.path))
            .collect::<Result<Vec<_>>>()?;
        self.catalog.add_paths_to_album(album, &paths)
    }

    /// Takes photos out of an album (they stay in the library), returning
    /// how many were in it.
    pub fn remove_from_album(&self, album: AlbumId, photos: &[Photo]) -> Result<usize> {
        let paths = photos
            .iter()
            .map(|photo| self.relative(&photo.path))
            .collect::<Result<Vec<_>>>()?;
        self.catalog.remove_paths_from_album(album, &paths)
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
        let albums = self.albums()?;
        let mut doomed = vec![album];
        let mut ix = 0;
        while let Some(&parent) = doomed.get(ix) {
            doomed.extend(child_albums(&albums, Some(parent)).map(|child| child.id));
            ix += 1;
        }
        self.catalog.delete_albums(&doomed)
    }
}

/// `name`, trimmed, if it's a name.
fn album_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("an album needs a name");
    }
    Ok(name)
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
    use std::sync::Arc;

    use super::super::{LibraryInfo, test_support};
    use super::*;

    fn album(id: AlbumId, parent: Option<AlbumId>, name: &str) -> Album {
        Album {
            id,
            parent,
            name: name.into(),
            photos: 0,
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
        assert_eq!(names(album_path(&albums, 4)), ["Trips", "Japan", "Kyoto"]);
        assert!(album_path(&albums, 99).is_empty());
        assert_eq!(
            names(child_albums(&albums, Some(3)).collect()),
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
}
