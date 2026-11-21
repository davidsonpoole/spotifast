//! Playlists seen as a shelf of albums.
//!
//! Spotify keeps songs in playlists, never albums, so an album added to a
//! playlist is all of its songs. A playlist shown as albums groups those
//! songs back into their albums and puts the album last listened to first.
//! When each album was last heard is kept here, since the local play
//! history and Spotify's recently played list both forget old plays.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::api::models::{Album, PlayHistory, PlayableItem, PlaylistItem};

/// When each album was last played, in seconds since the epoch, by album id.
#[derive(Default)]
pub struct AlbumPlays {
    last: HashMap<String, i64>,
    /// Counts changes, so a shelf drawn from these plays knows to sort again.
    revision: u64,
    /// Set when the in-memory map differs from the file.
    dirty: bool,
    /// Set by the file writer when a handed-over write failed.
    failed: Arc<AtomicBool>,
}

impl AlbumPlays {
    /// Reads the file, or starts empty if it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let last = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<HashMap<String, i64>>(&text).ok())
            .unwrap_or_default();
        Self {
            last,
            ..Self::default()
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// When the album was last played, if it ever was.
    pub fn last_played(&self, album_id: &str) -> Option<i64> {
        self.last.get(album_id).copied()
    }

    /// Writes down a play of a song from `album_id` at `at`. An older play
    /// never moves an album back.
    pub fn note(&mut self, album_id: &str, at: i64) {
        if album_id.is_empty() {
            return;
        }
        let last = self.last.entry(album_id.to_string()).or_insert(i64::MIN);
        if at > *last {
            *last = at;
            self.dirty = true;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Takes in the albums of a list of plays, from here or from Spotify.
    pub fn absorb(&mut self, plays: &[PlayHistory]) {
        for play in plays {
            let Some(album) = play.track.album.as_ref() else {
                continue;
            };
            let Some(at) = play
                .played_at
                .as_deref()
                .and_then(|at| at.parse::<jiff::Timestamp>().ok())
            else {
                continue;
            };
            self.note(&album.id, at.as_second());
        }
    }

    pub fn clear(&mut self) {
        if !self.last.is_empty() {
            self.last.clear();
            self.dirty = true;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Hands the map to `files` if it changed since the last save. A write
    /// that fails leaves it to be written by the next save.
    pub fn save(&mut self, files: &fastframe_file::Writer, path: &Path) {
        let failed = self.failed.swap(false, Ordering::SeqCst);
        if !std::mem::take(&mut self.dirty) && !failed {
            return;
        }
        match serde_json::to_string(&self.last) {
            Ok(text) => {
                // Like the play history: replaced whole, off the UI thread,
                // and readable by its owner only.
                let failed = self.failed.clone();
                files.write_or(path, text, fastframe_file::Access::Private, move || {
                    failed.store(true, Ordering::SeqCst)
                });
            }
            Err(error) => {
                self.dirty = true;
                log::warn!("could not write the album plays: {error}");
            }
        }
    }
}

/// One album on a playlist's shelf.
#[derive(Clone, Debug)]
pub struct ShelfAlbum {
    pub album: Album,
    /// The playlist's songs from this album, in playlist order.
    pub uris: Vec<String>,
    /// Those of them Spotify can play: no local files or unavailable songs.
    pub playable: Vec<String>,
    /// When the album was last played, in seconds since the epoch.
    pub last_played: Option<i64>,
    /// When its newest song was added to the playlist.
    added_at: Option<i64>,
    /// Where its first song sits in the playlist.
    first: usize,
}

/// Groups a playlist's songs into albums: the album played most recently
/// first, then albums never played, newest addition first. Episodes and
/// songs that name no album are left out.
pub fn shelf(items: &[PlaylistItem], plays: &AlbumPlays) -> Vec<ShelfAlbum> {
    let mut albums: Vec<ShelfAlbum> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (position, row) in items.iter().enumerate() {
        let Some(PlayableItem::Track(track)) = row.playable() else {
            continue;
        };
        let Some(album) = track.album.as_ref().filter(|album| !album.id.is_empty()) else {
            continue;
        };
        let added_at = row
            .added_at
            .as_deref()
            .and_then(|at| at.parse::<jiff::Timestamp>().ok())
            .map(|at| at.as_second());
        let slot = *index.entry(album.id.as_str()).or_insert_with(|| {
            albums.push(ShelfAlbum {
                album: album.clone(),
                uris: Vec::new(),
                playable: Vec::new(),
                last_played: plays.last_played(&album.id),
                added_at: None,
                first: position,
            });
            albums.len() - 1
        });
        let entry = &mut albums[slot];
        entry.uris.push(track.uri.clone());
        if !row.is_local && !track.is_local && track.is_playable != Some(false) {
            entry.playable.push(track.uri.clone());
        }
        entry.added_at = entry.added_at.max(added_at);
        // A song can name its album with fewer details than another.
        if entry.album.images.is_empty() && !album.images.is_empty() {
            entry.album.images = album.images.clone();
        }
        if entry.album.artists.is_empty() && !album.artists.is_empty() {
            entry.album.artists = album.artists.clone();
        }
    }
    albums.sort_by(|a, b| {
        b.last_played
            .cmp(&a.last_played)
            .then(b.added_at.cmp(&a.added_at))
            .then(a.first.cmp(&b.first))
    });
    albums
}

/// The most songs one play hands over, as for any other list of songs.
const MOST_SONGS: usize = 500;

/// An album shuffle's play order: the albums in random order, each one's
/// songs back to back in playlist order. `first` names an album to start
/// with. Whole albums are taken until the list is as long as a play may
/// be; an album longer than that alone is cut short.
pub fn album_shuffle_order(
    shelf: &[ShelfAlbum],
    first: Option<&str>,
    rng: &mut impl rand::Rng,
) -> Vec<String> {
    use rand::seq::SliceRandom;
    let mut albums: Vec<&ShelfAlbum> = shelf
        .iter()
        .filter(|entry| !entry.playable.is_empty())
        .collect();
    albums.shuffle(rng);
    if let Some(first) = first
        && let Some(at) = albums.iter().position(|entry| entry.album.id == first)
    {
        let chosen = albums.remove(at);
        albums.insert(0, chosen);
    }
    let mut order = Vec::new();
    for entry in albums {
        if !order.is_empty() && order.len() + entry.playable.len() > MOST_SONGS {
            continue;
        }
        order.extend(entry.playable.iter().cloned());
    }
    order.truncate(MOST_SONGS);
    order
}

/// Whether an album matches a playlist filter, by its name or an artist's.
pub fn matches(album: &ShelfAlbum, needle: &str) -> bool {
    needle.is_empty()
        || album.album.name.to_lowercase().contains(needle)
        || album
            .album
            .artists
            .iter()
            .any(|artist| artist.name.to_lowercase().contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::Track;

    fn song(uri: &str, album: &str, added_at: &str) -> PlaylistItem {
        PlaylistItem {
            added_at: Some(added_at.to_string()),
            item: Some(PlayableItem::Track(Track {
                uri: uri.to_string(),
                album: Some(Album {
                    id: album.to_string(),
                    name: album.to_uppercase(),
                    uri: format!("spotify:album:{album}"),
                    ..Album::default()
                }),
                ..Track::default()
            })),
            ..PlaylistItem::default()
        }
    }

    fn play(album: &str, at: &str) -> PlayHistory {
        PlayHistory {
            track: Track {
                album: Some(Album {
                    id: album.to_string(),
                    ..Album::default()
                }),
                ..Track::default()
            },
            played_at: Some(at.to_string()),
            context: None,
        }
    }

    fn ids(shelf: &[ShelfAlbum]) -> Vec<&str> {
        shelf.iter().map(|entry| entry.album.id.as_str()).collect()
    }

    /// An album's songs are one entry, in playlist order.
    #[test]
    fn an_albums_songs_are_one_entry() {
        let items = vec![
            song("spotify:track:a1", "a", "2026-01-01T00:00:00Z"),
            song("spotify:track:a2", "a", "2026-01-01T00:00:00Z"),
            song("spotify:track:b1", "b", "2026-01-01T00:00:00Z"),
        ];
        let shelf = shelf(&items, &AlbumPlays::default());
        assert_eq!(shelf.len(), 2);
        assert_eq!(shelf[0].uris, ["spotify:track:a1", "spotify:track:a2"]);
    }

    /// The album played most recently comes first; albums never played
    /// follow, newest addition first.
    #[test]
    fn the_album_played_last_comes_first() {
        let items = vec![
            song("spotify:track:a1", "a", "2026-01-01T00:00:00Z"),
            song("spotify:track:b1", "b", "2026-01-02T00:00:00Z"),
            song("spotify:track:c1", "c", "2026-01-03T00:00:00Z"),
            song("spotify:track:d1", "d", "2026-01-04T00:00:00Z"),
        ];
        let mut plays = AlbumPlays::default();
        plays.absorb(&[
            play("a", "2026-03-01T00:00:00Z"),
            play("c", "2026-03-02T00:00:00Z"),
        ]);
        assert_eq!(ids(&shelf(&items, &plays)), ["c", "a", "d", "b"]);
    }

    /// An older play reported later never moves an album back.
    #[test]
    fn an_older_play_never_moves_an_album_back() {
        let mut plays = AlbumPlays::default();
        plays.absorb(&[play("a", "2026-03-02T00:00:00Z")]);
        plays.absorb(&[play("a", "2026-03-01T00:00:00Z")]);
        let newer: jiff::Timestamp = "2026-03-02T00:00:00Z".parse().unwrap();
        assert_eq!(plays.last_played("a"), Some(newer.as_second()));
    }

    /// Episodes and songs with no album have no place on the shelf.
    #[test]
    fn songs_without_an_album_are_left_out() {
        let mut bare = song("spotify:track:x", "", "2026-01-01T00:00:00Z");
        if let Some(PlayableItem::Track(track)) = &mut bare.item {
            track.album = None;
        }
        let items = vec![bare, song("spotify:track:x", "", "2026-01-01T00:00:00Z")];
        assert!(shelf(&items, &AlbumPlays::default()).is_empty());
    }

    /// An album shuffle plays each album's songs back to back, in playlist
    /// order, and every album once; the order of albums changes.
    #[test]
    fn an_album_shuffle_keeps_each_album_together() {
        let items: Vec<PlaylistItem> = ["a", "b", "c", "d"]
            .iter()
            .flat_map(|album| {
                (1..=3).map(move |n| {
                    song(
                        &format!("spotify:track:{album}{n}"),
                        album,
                        "2026-01-01T00:00:00Z",
                    )
                })
            })
            .collect();
        let shelf = shelf(&items, &AlbumPlays::default());
        let mut orders = std::collections::HashSet::new();
        let mut rng = rand::rng();
        for _ in 0..40 {
            let order = album_shuffle_order(&shelf, None, &mut rng);
            assert_eq!(order.len(), 12, "every song once");
            for chunk in order.chunks(3) {
                let album = &chunk[0]["spotify:track:".len()..][..1];
                let expected: Vec<String> = (1..=3)
                    .map(|n| format!("spotify:track:{album}{n}"))
                    .collect();
                assert_eq!(chunk, expected.as_slice(), "an album plays in order");
            }
            orders.insert(order);
        }
        assert!(orders.len() > 1, "the albums come in different orders");
    }

    /// A chosen album plays first; the rest follow at random. Songs Spotify
    /// cannot play are left out.
    #[test]
    fn an_album_shuffle_can_start_with_a_chosen_album() {
        let mut items = vec![
            song("spotify:track:a1", "a", "2026-01-01T00:00:00Z"),
            song("spotify:track:b1", "b", "2026-01-01T00:00:00Z"),
            song("spotify:track:b2", "b", "2026-01-01T00:00:00Z"),
        ];
        if let Some(PlayableItem::Track(track)) = &mut items[2].item {
            track.is_playable = Some(false);
        }
        let shelf = shelf(&items, &AlbumPlays::default());
        let mut rng = rand::rng();
        for _ in 0..10 {
            let order = album_shuffle_order(&shelf, Some("b"), &mut rng);
            assert_eq!(order, ["spotify:track:b1", "spotify:track:a1"]);
        }
    }

    /// The file is read back as it was written.
    #[test]
    fn album_plays_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "spotifast-album-plays-{:016x}",
            rand::random::<u64>()
        ));
        let path = dir.join("album_plays.json");
        let mut plays = AlbumPlays::default();
        plays.note("a", 42);
        let files = fastframe_file::Writer::new();
        plays.save(&files, &path);
        assert!(files.flush(std::time::Duration::from_secs(10)));
        assert_eq!(AlbumPlays::load(&path).last_played("a"), Some(42));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
