//! The cover Spotify makes for a playlist without one of its own: the
//! first four albums' covers in quarters, or the first album's cover when
//! the playlist holds fewer albums. Spotify lists none for a new playlist
//! at first, and a list read over the playback session never carries one,
//! so Spotifast makes the same address itself. Spotify's image host draws
//! a mosaic of any four album covers it is given.

use crate::api::models::{Image, PlayableItem};

/// Albums a mosaic shows.
pub const ALBUMS: usize = 4;

const ALBUM_ART: &str = "https://i.scdn.co/image/ab67616d";
const MOSAIC_HOST: &str = "https://mosaic.scdn.co";
/// The size code of the 300-pixel album cover, the one mosaics are made of.
const MOSAIC_PIECE: &str = "00001e02";

/// The albums' covers among `songs`, each album once, in order.
pub fn album_covers<'a>(songs: impl IntoIterator<Item = &'a PlayableItem>) -> Vec<Vec<Image>> {
    let mut covers: Vec<Vec<Image>> = Vec::new();
    for song in songs {
        let PlayableItem::Track(track) = song else {
            continue;
        };
        let Some(images) = track
            .album
            .as_ref()
            .map(|album| &album.images)
            .filter(|images| !images.is_empty())
        else {
            continue;
        };
        if !covers.iter().any(|seen| same_cover(seen, images)) {
            covers.push(images.clone());
        }
    }
    covers
}

/// The cover Spotify would show for a playlist whose first albums have
/// `covers`: a mosaic of four, or else the first album's cover.
pub fn cover(covers: &[Vec<Image>]) -> Option<Vec<Image>> {
    mosaic(covers).or_else(|| covers.first().cloned())
}

/// Whether `images` is a cover Spotify made from a playlist's songs that
/// shows a single album: one a mosaic may replace once the playlist holds
/// four albums. A mosaic and a picture of the owner's own are not.
pub fn is_single_album(images: &[Image]) -> bool {
    !images.is_empty() && images.iter().all(|image| piece(&image.url).is_some())
}

fn mosaic(covers: &[Vec<Image>]) -> Option<Vec<Image>> {
    if covers.len() < ALBUMS {
        return None;
    }
    let pieces: Option<String> = covers[..ALBUMS]
        .iter()
        .map(|images| {
            let hash = images.iter().find_map(|image| piece(&image.url))?;
            Some(format!("ab67616d{MOSAIC_PIECE}{hash}"))
        })
        .collect();
    let pieces = pieces?;
    Some(
        [640, 300, 60]
            .into_iter()
            .map(|size| Image {
                url: format!("{MOSAIC_HOST}/{size}/{pieces}"),
                width: Some(size),
                height: Some(size),
            })
            .collect(),
    )
}

/// The part of an album cover's address that names the picture, the same
/// at every size: what follows the eight-digit size code.
fn piece(url: &str) -> Option<&str> {
    let rest = url.strip_prefix(ALBUM_ART)?;
    let hash = rest.get(8..)?;
    (rest.len() == 32 && rest.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
}

fn same_cover(a: &[Image], b: &[Image]) -> bool {
    match (
        a.iter().find_map(|image| piece(&image.url)),
        b.iter().find_map(|image| piece(&image.url)),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => a.first().map(|image| &image.url) == b.first().map(|image| &image.url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Album, Track};

    fn art(size: &str, hash: &str) -> Image {
        Image {
            url: format!("{ALBUM_ART}{size}{hash}"),
            width: None,
            height: None,
        }
    }

    fn song(hash: &str) -> PlayableItem {
        PlayableItem::Track(Track {
            album: Some(Album {
                images: vec![art("0000b273", hash), art("00004851", hash)],
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn hash(n: u8) -> String {
        format!("{n:x}").repeat(24)
    }

    #[test]
    fn four_albums_make_spotifys_mosaic() {
        let songs: Vec<PlayableItem> = [1, 1, 2, 3, 2, 4, 5]
            .into_iter()
            .map(|n| song(&hash(n)))
            .collect();
        let covers = album_covers(&songs);
        assert_eq!(covers.len(), 5, "each album once");
        let cover = cover(&covers).unwrap();
        let pieces: String = (1..=4)
            .map(|n| format!("ab67616d00001e02{}", hash(n)))
            .collect();
        assert_eq!(
            cover
                .iter()
                .map(|image| image.url.as_str())
                .collect::<Vec<_>>(),
            [640, 300, 60].map(|size| format!("https://mosaic.scdn.co/{size}/{pieces}"))
        );
        assert!(!is_single_album(&cover));
    }

    #[test]
    fn fewer_albums_show_the_first() {
        let songs: Vec<PlayableItem> = [3, 3, 7].into_iter().map(|n| song(&hash(n))).collect();
        let cover = cover(&album_covers(&songs)).unwrap();
        assert_eq!(cover[0].url, format!("{ALBUM_ART}0000b273{}", hash(3)));
        assert!(is_single_album(&cover));
        assert_eq!(super::cover(&[]), None);
    }

    #[test]
    fn an_album_cover_from_elsewhere_shows_alone() {
        let mut songs: Vec<PlayableItem> = (1..=4).map(|n| song(&hash(n))).collect();
        if let PlayableItem::Track(track) = &mut songs[2] {
            track.album.as_mut().unwrap().images = vec![Image {
                url: "https://example.com/cover.jpg".into(),
                width: None,
                height: None,
            }];
        }
        let cover = cover(&album_covers(&songs)).unwrap();
        assert_eq!(cover.len(), 2, "the first album's own cover");
        let own = [Image {
            url: "https://i.scdn.co/image/ab67706c0000da84".to_string() + &hash(9),
            width: None,
            height: None,
        }];
        assert!(!is_single_album(&own), "a picture of the owner's own");
    }
}
